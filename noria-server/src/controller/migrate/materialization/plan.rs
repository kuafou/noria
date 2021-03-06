use crate::controller::domain_handle::DomainHandle;
use crate::controller::inner::{graphviz, DomainReplies};
use crate::controller::keys;
use crate::controller::{Worker, WorkerIdentifier};
use dataflow::payload::{ReplayPathSegment, SourceSelection, TriggerEndpoint};
use dataflow::prelude::*;
use std::collections::{HashMap, HashSet};

pub(super) struct Plan<'a> {
    m: &'a mut super::Materializations,
    graph: &'a Graph,
    node: NodeIndex,
    domains: &'a mut HashMap<DomainIndex, DomainHandle>,
    workers: &'a HashMap<WorkerIdentifier, Worker>,
    partial: bool,

    tags: HashMap<Vec<usize>, Vec<(Tag, DomainIndex)>>,
    paths: HashMap<Tag, Vec<NodeIndex>>,
    pending: Vec<PendingReplay>,
}

#[derive(Debug)]
pub(super) struct PendingReplay {
    pub(super) tag: Tag,
    pub(super) source: LocalNodeIndex,
    pub(super) source_domain: DomainIndex,
    target_domain: DomainIndex,
}

impl<'a> Plan<'a> {
    pub(super) fn new(
        m: &'a mut super::Materializations,
        graph: &'a Graph,
        node: NodeIndex,
        domains: &'a mut HashMap<DomainIndex, DomainHandle>,
        workers: &'a HashMap<WorkerIdentifier, Worker>,
    ) -> Plan<'a> {
        let partial = m.partial.contains(&node);
        Plan {
            m,
            graph,
            node,
            domains,
            workers,

            partial,

            pending: Vec::new(),
            tags: Default::default(),
            paths: Default::default(),
        }
    }

    fn paths(&mut self, columns: &[usize]) -> Vec<Vec<(NodeIndex, Vec<Option<usize>>)>> {
        let graph = self.graph;
        let ni = self.node;
        let paths = keys::provenance_of(graph, ni, &columns[..], Self::on_join(&self.graph));

        // cut paths so they only reach to the the closest materialized node
        let mut paths: Vec<_> = paths
            .into_iter()
            .map(|path| -> Vec<_> {
                let mut found = false;
                let mut path: Vec<_> = path
                    .into_iter()
                    .enumerate()
                    .take_while(|&(i, (node, _))| {
                        // remember, the paths are "backwards", so the first node is target node
                        if i == 0 {
                            return true;
                        }

                        // keep taking until we get our first materialized node
                        // (`found` helps us emulate `take_while_inclusive`)
                        if found {
                            // we've already found a materialized node
                            return false;
                        }

                        if self.m.have.contains_key(&node) {
                            // we want to take this node, but not any later ones
                            found = true;
                        }
                        true
                    })
                    .map(|(_, segment)| segment)
                    .collect();
                path.reverse();
                path
            })
            .collect();

        // since we cut off part of each path, we *may* now have multiple paths that are the same
        // (i.e., if there was a union above the nearest materialization). this would be bad, as it
        // would cause a domain to request replays *twice* for a key from one view!
        paths.sort();
        paths.dedup();

        // all columns better resolve if we're doing partial
        assert!(!self.partial || paths.iter().all(|p| p[0].1.iter().all(|c| c.is_some())));

        paths
    }

    /// Finds the appropriate replay paths for the given index, and inform all domains on those
    /// paths about them. It also notes if any data backfills will need to be run, which is
    /// eventually reported back by `finalize`.
    pub(super) fn add(&mut self, index_on: Vec<usize>, replies: &mut DomainReplies) {
        if !self.partial && !self.paths.is_empty() {
            // non-partial views should not have one replay path per index. that would cause us to
            // replay several times, even though one full replay should always be sufficient.
            // we do need to keep track of the fact that there should be an index here though.
            self.tags.entry(index_on).or_default();
            return;
        }

        // inform domains about replay paths
        let mut tags = Vec::new();
        for path in self.paths(&index_on[..]) {
            let tag = self.m.next_tag();
            self.paths
                .insert(tag, path.iter().map(|&(ni, _)| ni).collect());

            // what key are we using for partial materialization (if any)?
            let mut partial = None;
            if self.partial {
                if let Some(&(_, ref cols)) = path.first() {
                    assert!(cols.iter().all(|c| c.is_some()));
                    let key: Vec<_> = cols.iter().map(|c| c.unwrap()).collect();
                    partial = Some(key);
                } else {
                    unreachable!();
                }
            }

            // first, find out which domains we are crossing
            let mut segments = Vec::new();
            let mut last_domain = None;
            for (node, cols) in path {
                let domain = self.graph[node].domain();
                if last_domain.is_none() || domain != last_domain.unwrap() {
                    segments.push((domain, Vec::new()));
                    last_domain = Some(domain);
                }

                let key = if self.partial {
                    Some(cols.into_iter().map(|c| c.unwrap()).collect::<Vec<_>>())
                } else {
                    None
                };

                segments.last_mut().unwrap().1.push((node, key));
            }

            info!(self.m.log, "domain replay path is {:?}", segments; "tag" => tag.id());

            // tell all the domains about their segment of this replay path
            let mut pending = None;
            let mut seen = HashSet::new();
            for (i, &(domain, ref nodes)) in segments.iter().enumerate() {
                // TODO:
                //  a domain may appear multiple times in this list if a path crosses into the same
                //  domain more than once. currently, that will cause a deadlock.
                if seen.contains(&domain) {
                    println!("{}", graphviz(&self.graph, true, &self.m));
                    crit!(self.m.log, "detected a-b-a domain replay path");
                    unimplemented!();
                }
                seen.insert(domain);

                // we're not replaying through the starter node
                let mut skip_first = 0;
                if i == 0 {
                    skip_first = 1;
                }

                // use the local index for each node
                let locals: Vec<_> = nodes
                    .iter()
                    .skip(skip_first)
                    .map(|&(ni, ref key)| ReplayPathSegment {
                        node: self.graph[ni].local_addr(),
                        partial_key: key.clone(),
                    })
                    .collect();

                // the first domain in the chain may *only* have the source node
                // in which case it doesn't need to know about the path
                if locals.is_empty() {
                    assert_eq!(i, 0);
                    continue;
                }

                // build the message we send to this domain to tell it about this replay path.
                let mut setup = box Packet::SetupReplayPath {
                    tag,
                    source: None,
                    path: locals,
                    notify_done: false,
                    trigger: TriggerEndpoint::None,
                };

                // the first domain also gets to know source node
                if i == 0 {
                    if let box Packet::SetupReplayPath { ref mut source, .. } = setup {
                        *source = Some(self.graph[nodes[0].0].local_addr());
                    }
                }

                if let Some(ref key) = partial {
                    // for partial materializations, nodes need to know how to trigger replays
                    if let box Packet::SetupReplayPath {
                        ref mut trigger, ..
                    } = setup
                    {
                        if segments.len() == 1 {
                            // replay is entirely contained within one domain
                            *trigger = TriggerEndpoint::Local(key.clone());
                        } else if i == 0 {
                            // first domain needs to be told about partial replay trigger
                            *trigger = TriggerEndpoint::Start(key.clone());
                        } else if i == segments.len() - 1 {
                            // if the source is sharded, we need to know whether we should ask all
                            // the shards, or just one. if the replay key is the same as the
                            // sharding key, we just ask one, and all is good. if the replay key
                            // and the sharding key differs, we generally have to query *all* the
                            // shards.
                            //
                            // there is, however, an exception to this: if we have two domains that
                            // have the same sharding, but a replay path between them on some other
                            // key than the sharding key, the right thing to do is to *only* query
                            // the same shard as ourselves. this is because any answers from other
                            // shards would necessarily just be with records that do not match our
                            // sharding key anyway, and that we should thus never see.
                            let src_sharding = self.graph[segments[0].1[0].0].sharded_by();
                            let shards = src_sharding.shards().unwrap_or(1);
                            let lookup_on_shard_key = match src_sharding {
                                Sharding::Random(..) => false,
                                Sharding::ByColumn(c, _) => {
                                    assert_eq!(key.len(), 1);
                                    c == key[0]
                                }
                                _ => true,
                            };

                            let selection = if lookup_on_shard_key {
                                // if we are not sharded, all is okay.
                                //
                                // if we are sharded:
                                //
                                //  - if there is a shuffle above us, a shard merger + sharder
                                //    above us will ensure that we hear the replay response.
                                //
                                //  - if there is not, we are sharded by the same column as the
                                //    source. this also means that the replay key in the
                                //    destination is the sharding key of the destination. to see
                                //    why, consider the case where the destination is sharded by x.
                                //    the source must also be sharded by x for us to be in this
                                //    case. we also know that the replay lookup key on the source
                                //    must be x since lookup_on_shard_key == true. since no shuffle
                                //    was introduced, src.x must resolve to dst.x assuming x is not
                                //    aliased in dst. because of this, it should be the case that
                                //    KeyShard == SameShard; if that were not the case, the value
                                //    in dst.x should never have reached dst in the first place.
                                SourceSelection::KeyShard(shards)
                            } else {
                                // replay key != sharding key
                                // normally, we'd have to query all shards, but if we are sharded
                                // the same as the source (i.e., there are no shuffles between the
                                // source and us), then we should *only* query the same shard of
                                // the source (since it necessarily holds all records we could
                                // possibly see).
                                if segments
                                    .iter()
                                    .flat_map(|s| s.1.iter())
                                    .any(|&(n, _)| self.graph[n].is_shard_merger())
                                {
                                    SourceSelection::AllShards(shards)
                                } else {
                                    SourceSelection::SameShard
                                }
                            };

                            debug!(self.m.log, "picked source selection policy"; "policy" => ?selection, "tag" => tag.id());
                            *trigger = TriggerEndpoint::End(selection, segments[0].0.clone());
                        }
                    } else {
                        unreachable!();
                    }
                } else {
                    // for full materializations, the last domain should report when it's done
                    if i == segments.len() - 1 {
                        if let box Packet::SetupReplayPath {
                            ref mut notify_done,
                            ..
                        } = setup
                        {
                            *notify_done = true;
                            assert!(pending.is_none());
                            pending = Some(PendingReplay {
                                tag,
                                source: self.graph[segments[0].1[0].0].local_addr(),
                                source_domain: segments[0].0,
                                target_domain: domain,
                            });
                        }
                    }
                }

                if i != segments.len() - 1 {
                    // since there is a later domain, the last node of any non-final domain
                    // must either be an egress or a Sharder. If it's an egress, we need
                    // to tell it about this replay path so that it knows
                    // what path to forward replay packets on.
                    let n = &self.graph[nodes.last().unwrap().0];
                    let workers = &self.workers;
                    if n.is_egress() {
                        self.domains
                            .get_mut(&domain)
                            .unwrap()
                            .send_to_healthy(
                                box Packet::UpdateEgress {
                                    node: n.local_addr(),
                                    new_tx: None,
                                    new_tag: Some((tag, segments[i + 1].1[0].0.into())),
                                },
                                workers,
                            )
                            .unwrap();
                    } else {
                        assert!(n.is_sharder());
                    }
                }

                trace!(self.m.log, "telling domain about replay path"; "domain" => domain.index());
                let ctx = self.domains.get_mut(&domain).unwrap();
                ctx.send_to_healthy(setup, self.workers).unwrap();
                replies.wait_for_acks(&ctx);
            }

            if !self.partial {
                // this path requires doing a replay and then waiting for the replay to finish
                self.pending
                    .push(pending.expect("no replay for full materialization?"));
            }
            tags.push((tag, last_domain.unwrap()));
        }

        self.tags.entry(index_on).or_default().extend(tags);
    }

    /// Instructs the target node to set up appropriate state for any new indices that have been
    /// added. For new indices added to full materializations, this may take some time (a
    /// re-indexing has to happen), whereas for new indices to partial views it should be nearly
    /// instantaneous.
    ///
    /// Returns a list of backfill replays that need to happen before the migration is complete.
    pub(super) fn finalize(mut self) -> Vec<PendingReplay> {
        use dataflow::payload::InitialState;

        // NOTE: we cannot use the impl of DerefMut here, since it (reasonably) disallows getting
        // mutable references to taken state.
        let s = self.graph[self.node]
            .with_reader(|r| {
                if self.partial {
                    assert!(r.is_materialized());

                    let last_domain = self.graph[self.node].domain();
                    let num_shards = self.domains[&last_domain].shards();

                    // since we're partially materializing a reader node,
                    // we need to give it a way to trigger replays.
                    InitialState::PartialGlobal {
                        gid: self.node,
                        cols: self.graph[self.node].fields().len(),
                        key: Vec::from(r.key().unwrap()),
                        trigger_domain: (last_domain, num_shards),
                    }
                } else {
                    InitialState::Global {
                        cols: self.graph[self.node].fields().len(),
                        key: Vec::from(r.key().unwrap()),
                        gid: self.node,
                    }
                }
            })
            .ok()
            .unwrap_or_else(|| {
                // not a reader
                if self.partial {
                    let indices = self
                        .tags
                        .drain()
                        .map(|(k, paths)| (k, paths.into_iter().map(|(tag, _)| tag).collect()))
                        .collect();
                    InitialState::PartialLocal(indices)
                } else {
                    let indices = self.tags.drain().map(|(k, _)| k).collect();
                    InitialState::IndexedLocal(indices)
                }
            });

        self.domains
            .get_mut(&self.graph[self.node].domain())
            .unwrap()
            .send_to_healthy(
                box Packet::PrepareState {
                    node: self.graph[self.node].local_addr(),
                    state: s,
                },
                self.workers,
            )
            .unwrap();

        if !self.partial {
            // we know that this must be a *new* fully materialized node:
            //
            //  - finalize() is only called by setup()
            //  - setup() is only called for existing nodes if they are partial
            //  - this branch has !self.partial
            //
            // if we're constructing a new view, there is no reason to replay any given path more
            // than once. we do need to be careful here though: the fact that the source and
            // destination of a path are the same does *not* mean that the path is the same (b/c of
            // unions), and we do not want to eliminate different paths!
            let mut distinct_paths = HashSet::new();
            let paths = &self.paths;
            self.pending.retain(|p| {
                // keep if this path is different
                distinct_paths.insert(&paths[&p.tag])
            });
            assert!(!self.pending.is_empty());
        } else {
            assert!(self.pending.is_empty());
        }
        self.pending
    }

    pub(super) fn on_join<'b>(
        graph: &'b Graph,
    ) -> impl FnMut(NodeIndex, &[Option<usize>], &[NodeIndex]) -> Option<NodeIndex> + 'b {
        move |node, cols, parents| {
            // this function should only be called when there's a choice
            assert!(parents.len() > 1);

            // and only internal nodes have multiple parents
            let n = &graph[node];
            assert!(n.is_internal());

            // keep track of remaining parents
            let mut parents = Vec::from(parents);

            // the node dictates that we *must* replay the state of some ancestor(s)
            let options = n
                .must_replay_among()
                .expect("join did not have must replay preference");
            parents.retain(|&parent| options.contains(&parent));
            assert!(!parents.is_empty());

            // we want to prefer source paths where we can translate all keys for the purposes of
            // partial -- but this only matters if we haven't already lost some keys.
            if cols.iter().all(|c| c.is_some()) {
                let first = cols[0].unwrap();

                let mut universal_src = Vec::new();
                for (src, col) in n.parent_columns(first) {
                    if src == node || col.is_none() {
                        continue;
                    }
                    if !options.contains(&src) {
                        // we can't choose to use this parent anyway
                        continue;
                    }

                    // if all other columns resolve into this src, then only keep those srcs
                    // XXX: this is pretty inefficient, but meh...
                    let also_to_src = cols.iter().skip(1).map(|c| c.unwrap()).all(|c| {
                        n.parent_columns(c)
                            .into_iter()
                            .find(|&(this_src, _)| this_src == src)
                            .and_then(|(_, c)| c)
                            .is_some()
                    });

                    if also_to_src {
                        universal_src.push(src);
                    }
                }

                if !universal_src.is_empty() {
                    parents = universal_src;
                }
            } else {
                // no ancestor has all the index columns, so any will do (and we won't be partial).
            }

            // if there is only one left, we don't really have a choice
            if parents.len() == 1 {
                // no need to pick
                return parents.pop();
            }

            // ensure that our choice of multiple possible parents is deterministic
            parents.sort_by_key(|p| p.index());

            // TODO:
            // if any required parent is empty, and we know we're building a full materialization,
            // the join must be empty (since outer join targets aren't required), and therefore
            // we can just pick that parent and get a free full materialization.

            // any choice is fine
            Some(parents[0])
        }
    }
}
