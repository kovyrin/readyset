use ops;
use query;
use shortcut;
use parking_lot;

use std::mem;
use std::ptr;
use std::sync::atomic;
use std::sync::atomic::AtomicPtr;

type S = (shortcut::Store<query::DataType>, LL);

/// This structure provides a storage mechanism that allows limited time-scoped queries. That is,
/// callers of `find()` may choose to *ignore* a suffix of the latest updates added with `add()`.
/// The results will be as if the `find()` was executed before those updates were received.
///
/// Only updates whose timestamp are higher than what was provided to the last call to `absorb()`
/// may be ignored. The backlog should periodically be absorbed back into the `Store` for
/// efficiency, as every find incurs a *linear scan* of all updates in the backlog.
pub struct BufferedStore {
    cols: usize,
    absorbed: atomic::AtomicIsize,
    store: parking_lot::RwLock<S>,
}

struct LL {
    // next is never mutatated, only overwritten or read
    next: AtomicPtr<LL>,
    entry: Option<(i64, Vec<ops::Record>)>,
}

impl LL {
    fn after(&self) -> Option<*mut LL> {
        let next = self.next.load(atomic::Ordering::Acquire);
        if next as *const LL == ptr::null() {
            // there's no next
            return None;
        }

        return Some(next);
    }

    fn take(&mut self) -> Option<(i64, Vec<ops::Record>)> {
        self.after().map(|next| {
            // steal the next and bypass
            let next = unsafe { Box::from_raw(next) };
            self.next.store(next.next.load(atomic::Ordering::Acquire),
                            atomic::Ordering::Release);
            next.entry.expect("only first LL should have None entry")
        })
    }
}

struct LLIter<'a>(&'a LL);
impl<'a> Iterator for LLIter<'a> {
    type Item = &'a (i64, Vec<ops::Record>);
    fn next(&mut self) -> Option<Self::Item> {
        use std::mem;

        loop {
            // we assume that the current node has already been yielded
            // so, we first advance, and then check for a value
            let next = self.0.after();

            if next.is_none() {
                // no next, so nothing more to iterate over
                return None;
            }

            self.0 = unsafe { mem::transmute(next.unwrap()) };

            // if we moved to a node that has a value, yield it
            if let Some(ref e) = self.0.entry {
                return Some(e);
            }
            // otherwise move again
        }
    }
}

fn lliter<'a>(lock: &'a parking_lot::RwLockReadGuard<'a, S>) -> LLIter<'a> {
    LLIter(&lock.1)
}

impl BufferedStore {
    /// Allocate a new buffered `Store`.
    pub fn new(cols: usize) -> BufferedStore {
        BufferedStore {
            cols: cols,
            absorbed: atomic::AtomicIsize::new(-1),
            store: parking_lot::RwLock::new((shortcut::Store::new(cols + 1 /* ts */),
                                             LL {
                next: AtomicPtr::new(unsafe { mem::transmute::<*const LL, *mut LL>(ptr::null()) }),
                entry: None,
            })),
        }
    }

    /// Absorb all updates in the backlog with a timestamp less than or equal to the given
    /// timestamp into the underlying `Store`. Note that this precludes calling `find()` with an
    /// `including` argument that is less than the given value.
    ///
    /// This operation will take time proportional to the number of entries in the backlog whose
    /// timestamp is less than or equal to the given timestamp.
    pub fn absorb(&self, including: i64) {
        let including = including as isize;
        if including <= self.absorbed.load(atomic::Ordering::Acquire) {
            return;
        }

        let mut store = self.store.write();
        self.absorbed.store(including, atomic::Ordering::Release);
        loop {
            match store.1.after() {
                Some(next) => {
                    // there's a next node to process
                    // check its timestamp
                    let n = unsafe { mem::transmute::<*mut LL, &LL>(next) };
                    assert!(n.entry.is_some());
                    if n.entry.as_ref().unwrap().0 as isize > including {
                        // it's too new, we're done
                        break;
                    }
                }
                None => break,
            }

            for r in store.1
                .take()
                .expect("no concurrent access, so if .after() is Some, so should .take()")
                .1
                .into_iter() {
                match r {
                    ops::Record::Positive(mut r, ts) => {
                        r.push(query::DataType::Number(ts));
                        store.0.insert(r);
                    }
                    ops::Record::Negative(r, ts) => {
                        // we need a cond that will match this row.
                        let conds = r.into_iter()
                            .enumerate()
                            .chain(Some((self.cols, query::DataType::Number(ts))).into_iter())
                            .map(|(coli, v)| {
                                shortcut::Condition {
                                    column: coli,
                                    cmp: shortcut::Comparison::Equal(shortcut::Value::Const(v)),
                                }
                            })
                            .collect::<Vec<_>>();

                        // however, multiple rows may have the same values as this row for every
                        // column. afaict, it is safe to delete any one of these rows. we do this
                        // by returning true for the first invocation of the filter function, and
                        // false for all subsequent invocations.
                        let mut first = true;
                        store.0.delete_filter(&conds[..], |_| {
                            if first {
                                first = false;
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
            }
        }
    }

    /// Add a new set of records to the backlog at the given timestamp.
    ///
    /// This method should never be called twice with the same timestamp, and the given timestamp
    /// must not yet have been absorbed.
    ///
    /// This method assumes that there are no other concurrent writers.
    pub unsafe fn add(&self, r: Vec<ops::Record>, ts: i64) {
        assert!(ts > self.absorbed.load(atomic::Ordering::Acquire) as i64);

        let add = Box::into_raw(Box::new(LL {
            next: AtomicPtr::new(mem::transmute::<*const LL, *mut LL>(ptr::null())),
            entry: Some((ts, r)),
        }));

        self.store.read().1.next.store(add, atomic::Ordering::Release);
    }

    /// Important and absorb a set of records at the given timestamp.
    pub fn batch_import(&self, rs: Vec<(Vec<query::DataType>, i64)>, ts: i64) {
        let mut lock = self.store.write();
        assert!(lock.1.next.load(atomic::Ordering::Acquire) as *const LL == ptr::null());
        assert!(self.absorbed.load(atomic::Ordering::Acquire) < ts as isize);
        for (mut row, ts) in rs.into_iter() {
            row.push(query::DataType::Number(ts));
            lock.0.insert(row);
        }
        self.absorbed.store(ts as isize, atomic::Ordering::Release);
    }

    fn extract_ts<'a>(&self, r: &'a [query::DataType]) -> (&'a [query::DataType], i64) {
        if let query::DataType::Number(ts) = r[self.cols] {
            (&r[0..self.cols], ts)
        } else {
            unreachable!()
        }
    }

    /// Find all entries that matched the given conditions just after the given point in time.
    ///
    /// Equivalent to running `find_and(&q.having[..], including)`, but projecting through the
    /// given query before results are returned. If not query is given, the returned records are
    /// cloned.
    pub fn find(&self,
                q: Option<query::Query>,
                including: Option<i64>)
                -> Vec<(Vec<query::DataType>, i64)> {
        self.find_and(q.as_ref().map(|q| &q.having[..]).unwrap_or(&[]),
                      including,
                      |rs| {
            rs.into_iter()
                .map(|(r, ts)| {
                    if let Some(ref q) = q {
                        (q.project(r), ts)
                    } else {
                        (r.iter().cloned().collect(), ts)
                    }
                })
                .collect()
        })
    }

    /// Find all entries that matched the given conditions just after the given point in time.
    ///
    /// Returned records are passed to `then` before being returned.
    ///
    /// This method will panic if the given timestamp falls before the last absorbed timestamp, as
    /// it cannot guarantee correct results in that case. Queries *at* the time of the last absorb
    /// are fine.
    ///
    /// Completes in `O(Store::find + b)` where `b` is the number of records in the backlog whose
    /// timestamp fall at or before the given timestamp.
    pub fn find_and<'a, F, T>(&self,
                              conds: &[shortcut::cmp::Condition<query::DataType>],
                              including: Option<i64>,
                              then: F)
                              -> T
        where T: 'a,
              F: 'a + FnOnce(Vec<(&[query::DataType], i64)>) -> T
    {
        let store = self.store.read();

        // okay, so we want to:
        //
        //  a) get the base results
        //  b) add any backlogged positives
        //  c) remove any backlogged negatives
        //
        // (a) is trivial (self.store.find)
        // we'll do (b) and (c) in two steps:
        //
        //  1) chain in all the positives in the backlog onto the base result iterator
        //  2) for each resulting row, check all backlogged negatives, and eliminate that result +
        //     the backlogged entry if there's a match.
        if including.is_none() {
            return then(store.0.find(conds).map(|r| self.extract_ts(r)).collect());
        }

        let including = including.unwrap();
        let absorbed = self.absorbed.load(atomic::Ordering::Acquire) as i64;
        if including == absorbed {
            return then(store.0.find(conds).map(|r| self.extract_ts(r)).collect());
        }

        assert!(including > absorbed);
        let mut relevant = lliter(&store)
            .take_while(|&&(ts, _)| ts <= including)
            .flat_map(|&(_, ref group)| group.iter())
            .filter(|r| conds.iter().all(|c| c.matches(&r.rec()[..])))
            .peekable();

        if relevant.peek().is_some() {
            let (positives, mut negatives): (_, Vec<_>) = relevant.partition(|r| r.is_positive());
            if negatives.is_empty() {
                then(store.0
                    .find(conds)
                    .map(|r| self.extract_ts(r))
                    .chain(positives.into_iter().map(|r| (r.rec(), r.ts())))
                    .collect())
            } else {
                then(store.0
                    .find(conds)
                    .map(|r| self.extract_ts(r))
                    .chain(positives.into_iter().map(|r| (r.rec(), r.ts())))
                    .filter_map(|(r, ts)| {
                        let revocation = negatives.iter()
                            .position(|neg| {
                                ts == neg.ts() &&
                                neg.rec().iter().enumerate().all(|(i, v)| &r[i] == v)
                            });

                        if let Some(revocation) = revocation {
                            // order of negatives doesn't matter, so O(1) swap_remove is fine
                            negatives.swap_remove(revocation);
                            None
                        } else {
                            Some((r, ts))
                        }
                    })
                    .collect())
            }
        } else {
            then(store.0.find(conds).map(|r| self.extract_ts(r)).collect())
        }
    }

    pub fn index<I: Into<shortcut::Index<query::DataType>>>(&self, column: usize, indexer: I) {
        self.store.write().0.index(column, indexer);
    }
}

mod tests {
    #[test]
    fn store_only() {
        let a1 = vec![1.into(), "a".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.absorb(0);
        assert_eq!(b.find(&[], Some(0)).len(), 1);
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
    }

    #[test]
    fn backlog_only() {
        let a1 = vec![1.into(), "a".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        assert_eq!(b.find(&[], Some(0)).len(), 1);
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
    }

    #[test]
    fn no_ts_ignores_backlog() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.absorb(0);
        assert_eq!(b.find(&[], None).len(), 1);
        assert!(b.find(&[], None)
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
    }

    #[test]
    fn store_and_backlog() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.absorb(0);
        assert_eq!(b.find(&[], Some(1)).len(), 2);
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn minimal_query() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.absorb(0);
        assert_eq!(b.find(&[], Some(0)).len(), 1);
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
    }

    #[test]
    fn non_minimal_query() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];
        let c3 = vec![3.into(), "c".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.add(vec![ops::Record::Positive(c3.clone(), 2)], 2);
        b.absorb(0);
        assert_eq!(b.find(&[], Some(1)).len(), 2);
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn absorb_negative_immediate() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.add(vec![ops::Record::Negative(a1.clone(), 0)], 2);
        b.absorb(2);
        assert_eq!(b.find(&[], Some(2)).len(), 1);
        assert!(b.find(&[], Some(2))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn absorb_negative_later() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.absorb(1);
        b.add(vec![ops::Record::Negative(a1.clone(), 0)], 2);
        b.absorb(2);
        assert_eq!(b.find(&[], Some(2)).len(), 1);
        assert!(b.find(&[], Some(2))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn query_negative() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];

        let mut b = BufferedStore::new(2);
        b.add(vec![ops::Record::Positive(a1.clone(), 0)], 0);
        b.add(vec![ops::Record::Positive(b2.clone(), 1)], 1);
        b.add(vec![ops::Record::Negative(a1.clone(), 0)], 2);
        assert_eq!(b.find(&[], Some(2)).len(), 1);
        assert!(b.find(&[], Some(2))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn absorb_multi() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];
        let c3 = vec![3.into(), "c".into()];

        let mut b = BufferedStore::new(2);

        b.add(vec![ops::Record::Positive(a1.clone(), 0), ops::Record::Positive(b2.clone(), 1)],
              0);
        b.absorb(0);
        assert_eq!(b.find(&[], Some(0)).len(), 2);
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));

        b.add(vec![ops::Record::Negative(a1.clone(), 0),
                   ops::Record::Positive(c3.clone(), 2),
                   ops::Record::Negative(c3.clone(), 2)],
              1);
        b.absorb(1);
        assert_eq!(b.find(&[], Some(1)).len(), 1);
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn query_multi() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];
        let c3 = vec![3.into(), "c".into()];

        let mut b = BufferedStore::new(2);

        b.add(vec![ops::Record::Positive(a1.clone(), 0), ops::Record::Positive(b2.clone(), 1)],
              0);
        assert_eq!(b.find(&[], Some(0)).len(), 2);
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 0 && r[0] == 1.into() && r[1] == "a".into()));
        assert!(b.find(&[], Some(0))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));

        b.add(vec![ops::Record::Negative(a1.clone(), 0),
                   ops::Record::Positive(c3.clone(), 2),
                   ops::Record::Negative(c3.clone(), 2)],
              1);
        assert_eq!(b.find(&[], Some(1)).len(), 1);
        assert!(b.find(&[], Some(1))
            .iter()
            .any(|&(r, ts)| ts == 1 && r[0] == 2.into() && r[1] == "b".into()));
    }

    #[test]
    fn query_complex() {
        let a1 = vec![1.into(), "a".into()];
        let b2 = vec![2.into(), "b".into()];
        let c3 = vec![3.into(), "c".into()];

        let mut b = BufferedStore::new(2);

        b.add(vec![ops::Record::Negative(a1.clone(), 0), ops::Record::Positive(b2.clone(), 1)],
              0);
        b.add(vec![ops::Record::Negative(b2.clone(), 1), ops::Record::Positive(c3.clone(), 2)],
              1);
        assert_eq!(b.find(&[], Some(2)), vec![(&*c3, 2)]);
    }
}
