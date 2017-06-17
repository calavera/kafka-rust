use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::hash::BuildHasherDefault;

use fnv::FnvHasher;

use client::{KafkaClient, FetchGroupOffset, FetchOffset};
use client::metadata::Topics;
use error::{ErrorKind, Result, KafkaCode};

use super::assignment::{Assignment, AssignmentRef, Assignments};
use super::config::Config;

pub type PartitionHasher = BuildHasherDefault<FnvHasher>;

// The "fetch state" for a particular topci partition.
#[derive(Debug)]
pub struct FetchState {
    /// ~ specifies the offset which to fetch from
    pub offset: i64,
    /// ~ specifies the max_bytes to be fetched
    pub max_bytes: i32,
}

#[derive(Debug, PartialEq, Eq, Hash)]
pub struct TopicPartition {
    /// ~ indirect reference to the topic through config.topic(..)
    pub topic_ref: AssignmentRef,
    /// ~ the partition to retry
    pub partition: i32,
}

#[derive(Debug)]
pub struct ConsumedOffset {
    /// ~ the consumed offset
    pub offset: i64,
    /// ~ true if the consumed offset is chnaged but not committed to
    /// kafka yet
    pub dirty: bool,
}

pub struct State {
    /// Contains the topic partitions the consumer is assigned to
    /// consume; this is a _read-only_ data structure
    pub assignments: Assignments,

    /// Contains the information relevant for the next fetch operation
    /// on the corresponding partitions
    pub fetch_offsets: HashMap<TopicPartition, FetchState, PartitionHasher>,

    /// Specifies partitions to be fetched on their own in the next
    /// poll request.
    pub retry_partitions: VecDeque<TopicPartition>,

    /// Contains the offsets of messages marked as "consumed" (to be
    /// committed)
    pub consumed_offsets: HashMap<TopicPartition, ConsumedOffset, PartitionHasher>,
}

impl<'a> fmt::Debug for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "State {{ assignments: {:?}, fetch_offsets: {:?}, retry_partitions: {:?}, \
                consumed_offsets: {:?} }}",
            self.assignments,
            self.fetch_offsets_debug(),
            TopicPartitionsDebug {
                state: self,
                tps: &self.retry_partitions,
            },
            self.consumed_offsets_debug()
        )
    }
}

impl State {
    pub fn new(
        client: &mut KafkaClient,
        config: &Config,
        assignments: Assignments,
    ) -> Result<State> {
        let (consumed_offsets, fetch_offsets) = {
            let subscriptions = {
                let xs = assignments.as_slice();
                let mut subs = Vec::with_capacity(xs.len());
                for x in xs {
                    subs.push(try!(determine_partitions(x, client.topics())));
                }
                subs
            };
            let n = subscriptions.iter().map(|s| s.partitions.len()).fold(
                0,
                |acc, n| {
                    acc + n
                },
            );
            let consumed =
                try!(load_consumed_offsets(client, &config.group, &assignments, &subscriptions, n));
            let fetch_next =
                try!(load_fetch_states(client, config, &assignments, &subscriptions, &consumed, n));
            (consumed, fetch_next)
        };
        Ok(State {
            assignments: assignments,
            fetch_offsets: fetch_offsets,
            retry_partitions: VecDeque::new(),
            consumed_offsets: consumed_offsets,
        })
    }

    pub fn topic_name(&self, assignment: AssignmentRef) -> &str {
        self.assignments[assignment].topic()
    }

    pub fn topic_ref(&self, name: &str) -> Option<AssignmentRef> {
        self.assignments.topic_ref(name)
    }

    /// Returns a wrapper around `self.fetch_offsets` for nice dumping
    /// in debug messages
    pub fn fetch_offsets_debug<'a>(&'a self) -> OffsetsMapDebug<'a, FetchState> {
        OffsetsMapDebug {
            state: self,
            offsets: &self.fetch_offsets,
        }
    }

    pub fn consumed_offsets_debug<'a>(&'a self) -> OffsetsMapDebug<'a, ConsumedOffset> {
        OffsetsMapDebug {
            state: self,
            offsets: &self.consumed_offsets,
        }
    }
}

// Specifies the actual partitions of a topic to be consumed
struct Subscription<'a> {
    assignment: &'a Assignment, // the assignment - user configuration
    partitions: Vec<i32>, // the actual partitions to be consumed
}

/// Determines the partitions to be consumed according to the
/// specified topic and requested partitions configuration. Returns an
/// ordered list of the partition ids to consume.
fn determine_partitions<'a>(
    assignment: &'a Assignment,
    metadata: Topics,
) -> Result<Subscription<'a>> {
    let topic = assignment.topic();
    let req_partitions = assignment.partitions();
    let avail_partitions = match metadata.partitions(topic) {
        // ~ fail if the underlying topic is unkonwn to the given client
        None => {
            debug!("determine_partitions: no such topic: {} (all metadata: {:?})", topic, metadata);
            bail!(ErrorKind::Kafka(KafkaCode::UnknownTopicOrPartition));
        }
        Some(tp) => tp,
    };
    let ps = if req_partitions.is_empty() {
        // ~ no partitions configured ... use all available
        let mut ps: Vec<i32> = Vec::with_capacity(avail_partitions.len());
        for p in avail_partitions {
            ps.push(p.id());
        }
        ps
    } else {
        // ~ validate that all partitions we're going to consume are
        // available
        let mut ps: Vec<i32> = Vec::with_capacity(req_partitions.len());
        for &p in req_partitions {
            match avail_partitions.partition(p) {
                None => {
                    debug!(
                        "determine_partitions: no such partition: \"{}:{}\" \
                            (all metadata: {:?})",
                        topic,
                        p,
                        metadata
                    );
                    bail!(ErrorKind::Kafka(KafkaCode::UnknownTopicOrPartition));
                }
                Some(_) => ps.push(p),
            };
        }
        ps
    };
    Ok(Subscription {
        assignment: assignment,
        partitions: ps,
    })
}

// Fetches the so-far commited/consumed offsets for the configured
// group/topic/partitions.
fn load_consumed_offsets(
    client: &mut KafkaClient,
    group: &str,
    assignments: &Assignments,
    subscriptions: &[Subscription],
    result_capacity: usize,
) -> Result<HashMap<TopicPartition, ConsumedOffset, PartitionHasher>> {
    assert!(!subscriptions.is_empty());
    // ~ pre-allocate the right size
    let mut offs = HashMap::with_capacity_and_hasher(result_capacity, PartitionHasher::default());
    // ~ no group, no persisted consumed offsets
    if group.is_empty() {
        return Ok(offs);
    }
    // ~ otherwise try load them for the group
    let tpos = try!(client.fetch_group_offsets(
        group,
        subscriptions.iter().flat_map(|s| {
            let topic = s.assignment.topic();
            s.partitions.iter().map(
                move |&p| FetchGroupOffset::new(topic, p),
            )
        }),
    ));
    for (topic, pos) in tpos {
        for po in pos {
            if po.offset != -1 {
                offs.insert(
                    TopicPartition {
                        topic_ref: assignments.topic_ref(&topic).expect("non-assigned topic"),
                        partition: po.partition,
                    },
                    ConsumedOffset {
                        offset: po.offset,
                        dirty: false,
                    },
                );
            }
        }
    }
    Ok(offs)
}

/// Fetches the "next fetch" offsets/states based on the specified
/// subscriptions and the given consumed offsets.
fn load_fetch_states(
    client: &mut KafkaClient,
    config: &Config,
    assignments: &Assignments,
    subscriptions: &[Subscription],
    consumed_offsets: &HashMap<TopicPartition, ConsumedOffset, PartitionHasher>,
    result_capacity: usize,
) -> Result<HashMap<TopicPartition, FetchState, PartitionHasher>> {
    fn load_partition_offsets(
        client: &mut KafkaClient,
        topics: &[&str],
        offset: FetchOffset,
    ) -> Result<HashMap<String, HashMap<i32, i64, PartitionHasher>>> {
        let toffs = try!(client.fetch_offsets(topics, offset));
        let mut m = HashMap::with_capacity(toffs.len());
        for (topic, poffs) in toffs {
            let mut pidx =
                HashMap::with_capacity_and_hasher(poffs.len(), PartitionHasher::default());

            for poff in poffs {
                pidx.insert(poff.partition, poff.offset);
            }

            m.insert(topic, pidx);
        }
        Ok(m)
    }

    let mut fetch_offsets =
        HashMap::with_capacity_and_hasher(result_capacity, PartitionHasher::default());
    let max_bytes = client.fetch_max_bytes_per_partition();
    let subscription_topics: Vec<_> = subscriptions.iter().map(|s| s.assignment.topic()).collect();
    if consumed_offsets.is_empty() {
        // ~ if there are no offsets on behalf of the consumer
        // group - if any - we can directly use the fallback offsets.
        let offsets =
            try!(load_partition_offsets(client, &subscription_topics, config.fallback_offset));
        for s in subscriptions {
            let topic_ref = assignments.topic_ref(s.assignment.topic()).expect(
                "unassigned subscription",
            );
            match offsets.get(s.assignment.topic()) {
                None => {
                    debug!(
                        "load_fetch_states: failed to load fallback offsets for: {}",
                        s.assignment.topic()
                    );
                    bail!(ErrorKind::Kafka(KafkaCode::UnknownTopicOrPartition));
                }
                Some(offsets) => {
                    for p in &s.partitions {
                        fetch_offsets.insert(
                            TopicPartition {
                                topic_ref: topic_ref,
                                partition: *p,
                            },
                            FetchState {
                                offset: *offsets.get(p).unwrap_or(&-1),
                                max_bytes: max_bytes,
                            },
                        );
                    }
                }
            }
        }
    } else {
        // fetch the earliest and latest available offsets
        let latest =
            try!(load_partition_offsets(client, &subscription_topics, FetchOffset::Latest));
        let earliest =
            try!(load_partition_offsets(client, &subscription_topics, FetchOffset::Earliest));
        // ~ for each subscribed partition if we have a
        // consumed_offset verify it is in the earliest/latest range
        // and use that consumed_offset+1 as the fetch_message.
        for s in subscriptions {
            let topic_ref = assignments.topic_ref(s.assignment.topic()).expect(
                "unassigned subscription",
            );
            for p in &s.partitions {
                let l_off = *latest
                    .get(s.assignment.topic())
                    .and_then(|ps| ps.get(p))
                    .unwrap_or(&-1);
                let e_off = *earliest
                    .get(s.assignment.topic())
                    .and_then(|ps| ps.get(p))
                    .unwrap_or(&-1);

                let tp = TopicPartition {
                    topic_ref: topic_ref,
                    partition: *p,
                };
                let offset = match consumed_offsets.get(&tp) {
                    Some(co) if co.offset >= e_off && co.offset < l_off => co.offset + 1,
                    _ => {
                        match config.fallback_offset {
                            FetchOffset::Latest => l_off,
                            FetchOffset::Earliest => e_off,
                            _ => {
                                debug!(
                                    "cannot determine fetch offset \
                                        (group: {} / topic: {} / partition: {})",
                                    &config.group,
                                    s.assignment.topic(),
                                    p
                                );
                                bail!(ErrorKind::Kafka(KafkaCode::Unknown));
                            }
                        }
                    }
                };
                fetch_offsets.insert(
                    tp,
                    FetchState {
                        offset: offset,
                        max_bytes: max_bytes,
                    },
                );
            }
        }
    }
    Ok(fetch_offsets)
}

pub struct OffsetsMapDebug<'a, T: 'a> {
    state: &'a State,
    offsets: &'a HashMap<TopicPartition, T, PartitionHasher>,
}

impl<'a, T: fmt::Debug + 'a> fmt::Debug for OffsetsMapDebug<'a, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "{{"));
        for (i, (tp, v)) in self.offsets.iter().enumerate() {
            if i != 0 {
                try!(write!(f, ", "));
            }
            let topic = self.state.topic_name(tp.topic_ref);
            try!(write!(f, "\"{}:{}\": {:?}", topic, tp.partition, v));
        }
        write!(f, "}}")
    }
}

struct TopicPartitionsDebug<'a> {
    state: &'a State,
    tps: &'a VecDeque<TopicPartition>,
}

impl<'a> fmt::Debug for TopicPartitionsDebug<'a> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        try!(write!(f, "["));
        for (i, tp) in self.tps.iter().enumerate() {
            if i != 0 {
                try!(write!(f, " ,"));
            }
            try!(write!(f, "\"{}:{}\"", self.state.topic_name(tp.topic_ref), tp.partition));
        }
        write!(f, "]")
    }
}
