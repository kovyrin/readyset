use flow;
use query;
use ops;
use backlog;

use shortcut;

use std::sync;
use std::collections::HashMap;

pub type Params = Vec<shortcut::Value<query::DataType>>;
pub type AQ = HashMap<flow::NodeIndex,
                      Box<Fn(Params) -> Box<Iterator<Item = Vec<query::DataType>>> + Send + Sync>>;
pub type Datas<'a> = Box<Iterator<Item = Vec<query::DataType>> + 'a>;

/// `NodeOp` represents the internal operations performed by a node. This trait is very similar to
/// `flow::View`, and for good reason. This is effectively the behavior of a node when there is no
/// materialization, and no multithreading. Those features are both added on by Node to expose a
/// `flow::View`. A NodeOp should not have, nor need, any mutable state (which is why all receivers
/// are `&`, not `&mut`). Instead, `self` should hold the node's internal configuration (e.g., what
/// fields to join on, how to aggregate).
///
/// It *might* be possible to merge forward and query (after all, they do very similar things), but
/// I haven't found a nice interface for that yet.
pub trait NodeOp {
    /// When a new update comes in to a node, this function is called with that update. The
    /// resulting update (if any) is sent to all child nodes. If the node is materialized, and the
    /// resulting update contains positive or negative records, the materialized state is updated
    /// appropriately.
    fn forward(&self,
               ops::Update,
               flow::NodeIndex,
               Option<&backlog::BufferedStore>,
               &AQ)
               -> Option<ops::Update>;

    /// Called whenever this node is being queried for records, and it is not materialized. The
    /// node should use the list of ancestor query functions to fetch relevant data from upstream,
    /// and emit resulting records as they come in. Note that there may be no query, in which case
    /// all records should be returned.
    fn query<'a>(&'a self, Option<&query::Query>, sync::Arc<AQ>) -> Datas<'a>;
}

pub struct Node<O: NodeOp + Sized + 'static + Send + Sync> {
    fields: Vec<String>,
    data: sync::Arc<Option<sync::RwLock<backlog::BufferedStore>>>,
    inner: sync::Arc<O>,
    local_ts: sync::atomic::AtomicIsize,
}

/// This is a somewhat nasty trick to allow an iterator to hold a read lock.
/// Basically, since the lock is in an Arc, we know that the lock will never be dropped.
/// We can thus just take a read lock, and keep it around (transmuting it to 'static) until the
/// iterator has completed. We *aren't* allowed to return a reference from the iterator though,
/// since the value might already be gone by the time the consumer of the iterator tries to read
/// the reference. In particular, the read lock guard might have dropped, causing a data race, or
/// the Arc itself might have been dropped, which might case a use-after-free.
struct ArcStoreRef {
    _keep: sync::Arc<Option<sync::RwLock<backlog::BufferedStore>>>,
    _lock: sync::RwLockReadGuard<'static, backlog::BufferedStore>,
    iter: Box<Iterator<Item = &'static [query::DataType]>>,
}

impl !Send for ArcStoreRef {}

impl ArcStoreRef {
    pub fn new(src: sync::Arc<Option<sync::RwLock<backlog::BufferedStore>>>,
               conds: &[shortcut::Condition<query::DataType>],
               ts: i64)
               -> Option<ArcStoreRef> {
        use std::mem;
        use std::ops::Deref;

        if src.is_none() {
            return None;
        }

        let rlock: sync::RwLockReadGuard<'static, backlog::BufferedStore> = {
            let rlock = src.deref().as_ref().unwrap().read().unwrap();
            // safe to make 'static because we keep the Arc around
            // it's really 'as-long-as-this-struct-is-around
            unsafe { mem::transmute(rlock) }
        };

        let iter = {
            let iter = rlock.find(conds, ts);
            // safe to make 'static because we keep the rlock around, and never expose refs to things
            // yielded by the iter; they are always made owned before returning them (and thus before
            // self can be dropped). as above, it is really 'as-long-as-this-struct-is-around.
            unsafe { mem::transmute(iter) }
        };

        Some(ArcStoreRef {
            _keep: src,
            _lock: rlock,
            iter: iter,
        })
    }

    pub fn start<F: Fn(&[query::DataType]) -> Vec<query::DataType>>(self,
                                                                    to_owned: F)
                                                                    -> ArcStoreIterator<F> {
        ArcStoreIterator {
            store: self,
            to_owned: to_owned,
        }
    }
}

struct ArcStoreIterator<F: Fn(&[query::DataType]) -> Vec<query::DataType>> {
    store: ArcStoreRef,
    to_owned: F,
}

impl<F: Fn(&[query::DataType]) -> Vec<query::DataType>> Iterator for ArcStoreIterator<F> {
    type Item = Vec<query::DataType>;
    fn next(&mut self) -> Option<Self::Item> {
        self.store.iter.next().and_then(|v| Some((self.to_owned)(v)))
    }
}

impl<O> flow::View<query::Query> for Node<O>
    where O: NodeOp + Sized + 'static + Send + Sync
{
    type Update = ops::Update;
    type Data = Vec<query::DataType>;
    type Params = Params;

    fn find<'a>(&'a self,
                aqs: sync::Arc<AQ>,
                q: Option<&query::Query>,
                mut p: Params)
                -> Box<Iterator<Item = Self::Data> + 'a> {

        // insert all the query arguments
        p.reverse(); // so we can pop below
        let mut q_cur = q.and_then(|q| Some(q.clone()));
        if let Some(ref mut q_cur) = q_cur {
            for c in q_cur.having.iter_mut() {
                match c.cmp {
                    shortcut::Comparison::Equal(
                        ref mut v @ shortcut::Value::Const(
                            query::DataType::None
                        )
                    ) => {
                        *v = p.pop().expect("not enough query parameters were given");
                    }
                    _ => (),
                }
            }
        }

        // find and return matching rows
        if self.data.is_some() {
            // TODO: not local time
            let ts = self.local_ts.load(sync::atomic::Ordering::SeqCst) as i64;

            let data = self.data.clone();
            if let Some(q) = q {
                let q = q.clone();
                Box::new(ArcStoreRef::new(data, &q.having[..], ts)
                    .unwrap()
                    .start(move |r| q.project(r)))
            } else {
                Box::new(ArcStoreRef::new(data, &[], ts)
                    .unwrap()
                    .start(|r| r.iter().cloned().collect()))
            }
        } else {
            // we are not materialized --- query
            Box::new(self.inner.query(q, aqs))
        }
    }

    fn process(&self,
               u: Self::Update,
               src: flow::NodeIndex,
               aqs: sync::Arc<AQ>)
               -> Option<Self::Update> {
        use std::ops::Deref;
        let mut data = self.data.deref().as_ref().and_then(|l| Some(l.write().unwrap()));

        let new_u = self.inner.forward(u, src, data.as_ref().and_then(|d| Some(&**d)), &*aqs);
        if let Some(ref new_u) = new_u {
            // TODO: we should obviously not be using a local ts here, because then specifying the
            // ts argument for find() given an update from another Node won't make any sense.
            let ts = self.local_ts.fetch_add(1, sync::atomic::Ordering::SeqCst) as i64;
            match *new_u {
                ops::Update::Records(ref rs) => {
                    if let Some(ref mut data) = data {
                        data.add(rs.clone(), ts);
                    }
                }
            }

            if let Some(ref mut data) = data {
                data.absorb(ts);
            }
        }
        new_u
    }
}

pub fn new<'a, S: ?Sized, O>(fields: &[&'a S], materialized: bool, inner: O) -> Node<O>
    where &'a S: Into<String>,
          O: NodeOp + Sized + 'static + Send + Sync
{
    let mut data = None;
    if materialized {
        data = Some(sync::RwLock::new(backlog::BufferedStore::new(fields.len())));
    }

    Node {
        local_ts: sync::atomic::AtomicIsize::new(0),
        fields: fields.iter().map(|&s| s.into()).collect(),
        data: sync::Arc::new(data),
        inner: sync::Arc::new(inner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flow;
    use ops;
    use query;
    use backlog;

    use std::time;
    use std::sync;
    use std::thread;

    struct Tester(i64);

    impl NodeOp for Tester {
        fn forward(&self,
                   u: ops::Update,
                   _: flow::NodeIndex,
                   _: Option<&backlog::BufferedStore>,
                   _: &AQ)
                   -> Option<ops::Update> {
            // forward
            match u {
                ops::Update::Records(mut rs) => {
                    if let Some(ops::Record::Positive(r)) = rs.pop() {
                        if let query::DataType::Number(r) = r[0] {
                            Some(
                                ops::Update::Records(
                                    vec![ops::Record::Positive(vec![(r + self.0).into()])]
                                )
                            )
                        } else {
                            unreachable!();
                        }
                    } else {
                        unreachable!();
                    }
                }
            }
        }

        fn query<'a>(&'a self, _: Option<&query::Query>, aqs: sync::Arc<AQ>) -> Datas<'a> {
            // query all ancestors, emit r + c for each
            let mut iter = Box::new(None.into_iter()) as Datas;
            for (_, aq) in aqs.iter() {
                iter = Box::new(iter.chain(aq(vec![])));
            }

            let c = self.0;
            Box::new(iter.map(move |r| {
                if let query::DataType::Number(r) = r[0] {
                    vec![(r + c).into()]
                } else {
                    unreachable!();
                }
            }))
        }
    }

    #[test]
    fn materialized() {
        use std::collections::HashSet;

        // set up graph
        let mut g = flow::FlowGraph::new();
        let all = query::Query {
            select: vec![true],
            having: vec![],
        };
        let a = g.incorporate(new(&["x"], true, Tester(1)), vec![]);
        let b = g.incorporate(new(&["x"], true, Tester(2)), vec![]);
        let c = g.incorporate(new(&["x"], true, Tester(4)),
                              vec![(all.clone(), a), (all.clone(), b)]);
        let d = g.incorporate(new(&["x"], true, Tester(8)), vec![(all.clone(), c)]);
        let (put, get) = g.run(10);

        // send a value
        put[&a].send(ops::Update::Records(vec![ops::Record::Positive(vec![1.into()])])).unwrap();

        // state should now be:
        // a = [2]
        // b = []
        // c = [6]
        // d = [14]

        // give it some time to propagate
        thread::sleep(time::Duration::new(0, 1_000_000));

        // send another in
        put[&b].send(ops::Update::Records(vec![ops::Record::Positive(vec![16.into()])])).unwrap();

        // state should now be:
        // a = [2]
        // b = [18]
        // c = [6, 22]
        // d = [14, 30]

        // give it some time to propagate
        thread::sleep(time::Duration::new(0, 1_000_000));

        // check state
        // a
        let set = get[&a](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&2));
        // b
        let set = get[&b](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&18), format!("18 not in {:?}", set));
        // c
        let set = get[&c](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&6), format!("6 not in {:?}", set));
        assert!(set.contains(&22), format!("22 not in {:?}", set));
        // d
        let set = get[&d](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&14), format!("14 not in {:?}", set));
        assert!(set.contains(&30), format!("30 not in {:?}", set));
    }

    #[test]
    fn not_materialized() {
        use std::collections::HashSet;

        // set up graph
        let mut g = flow::FlowGraph::new();
        let all = query::Query {
            select: vec![true],
            having: vec![],
        };
        let a = g.incorporate(new(&["x"], true, Tester(1)), vec![]);
        let b = g.incorporate(new(&["x"], true, Tester(2)), vec![]);
        let c = g.incorporate(new(&["x"], false, Tester(4)),
                              vec![(all.clone(), a), (all.clone(), b)]);
        let d = g.incorporate(new(&["x"], false, Tester(8)), vec![(all.clone(), c)]);
        let (put, get) = g.run(10);

        // send a value
        put[&a].send(ops::Update::Records(vec![ops::Record::Positive(vec![1.into()])])).unwrap();

        // state should now be:
        // a = [2]
        // b = []
        // c = [6]
        // d = [14]

        // give it some time to propagate
        thread::sleep(time::Duration::new(0, 1_000_000));

        // send another in
        put[&b].send(ops::Update::Records(vec![ops::Record::Positive(vec![16.into()])])).unwrap();

        // state should now be:
        // a = [2]
        // b = [18]
        // c = [6, 22]
        // d = [14, 30]

        // give it some time to propagate
        thread::sleep(time::Duration::new(0, 1_000_000));

        // check state
        // a
        let set = get[&a](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&2));
        // b
        let set = get[&b](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&18), format!("18 not in {:?}", set));
        // c
        let set = get[&c](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&6), format!("6 not in {:?}", set));
        assert!(set.contains(&22), format!("22 not in {:?}", set));
        // d
        let set = get[&d](vec![]).map(|mut v| v.pop().unwrap().into()).collect::<HashSet<i64>>();
        assert!(set.contains(&14), format!("14 not in {:?}", set));
        assert!(set.contains(&30), format!("30 not in {:?}", set));
    }
}
