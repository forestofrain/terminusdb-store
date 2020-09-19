//! builder frontend for constructing new layers
//!
//! `base` and `child` contain their own layer builders, but these are
//! not very easy to use. They require one to first insert all new
//! dictionary entries in sorted order, and then all numerical triple
//! additions/removals in sorted order
//!
//! The layer builder implemented here will instead take triples in
//! any format (numerical, string, or a mixture), store them in
//! memory, then does the required sorting and id conversion on
//! commit.
use super::base::*;
use super::child::*;
use super::layer::*;
use crate::storage::*;
use futures::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use rayon;
use rayon::prelude::*;

/// A layer builder trait with no generic typing.
///
/// Lack of generic types allows layer builders with different storage
/// backends to be handled by trait objects of this type.
pub trait LayerBuilder: Send + Sync {
    /// Returns the name of the layer being built
    fn name(&self) -> [u32; 5];
    /// Add a string triple
    fn add_string_triple(&mut self, triple: StringTriple);
    /// Add an id triple
    fn add_id_triple(&mut self, triple: IdTriple);
    /// Remove a string triple
    fn remove_string_triple(&mut self, triple: StringTriple);
    /// Remove an id triple
    fn remove_id_triple(&mut self, triple: IdTriple);
    /// Commit the layer to storage
    fn commit(self) -> Box<dyn Future<Output = Result<(), std::io::Error>> + Send>;
    /// Commit a boxed layer to storage
    fn commit_boxed(self: Box<Self>) -> Box<dyn Future<Output = Result<(), std::io::Error>> + Send>;
}

/// A layer builder
///
/// `SimpleLayerBuilder` provides methods for adding and removing
/// triples, and for committing the layer builder to storage.
#[derive(Clone)]
pub struct SimpleLayerBuilder<F: 'static + FileLoad + FileStore + Clone> {
    name: [u32; 5],
    parent: Option<Arc<dyn Layer>>,
    files: LayerFiles<F>,
    additions: Vec<StringTriple>,
    id_additions: Vec<IdTriple>,
    removals: Vec<StringTriple>,
    id_removals: Vec<IdTriple>,
}

impl<F: 'static + FileLoad + FileStore + Clone> SimpleLayerBuilder<F> {
    /// Construct a layer builder for a base layer
    pub fn new(name: [u32; 5], files: BaseLayerFiles<F>) -> Self {
        Self {
            name,
            parent: None,
            files: LayerFiles::Base(files),
            additions: Vec::new(),
            id_additions: Vec::with_capacity(0),
            removals: Vec::new(),
            id_removals: Vec::with_capacity(0),
        }
    }

    /// Construct a layer builder for a child layer
    pub fn from_parent(name: [u32; 5], parent: Arc<dyn Layer>, files: ChildLayerFiles<F>) -> Self {
        Self {
            name,
            parent: Some(parent),
            files: LayerFiles::Child(files),
            additions: Vec::new(),
            id_additions: Vec::new(),
            removals: Vec::new(),
            id_removals: Vec::new(),
        }
    }
}

impl<F: 'static + FileLoad + FileStore + Clone> LayerBuilder for SimpleLayerBuilder<F> {
    fn name(&self) -> [u32; 5] {
        self.name
    }

    fn add_string_triple(&mut self, triple: StringTriple) {
        self.additions.push(triple);
    }

    fn add_id_triple(&mut self, triple: IdTriple) {
        self.id_additions.push(triple);
    }

    fn remove_string_triple(&mut self, triple: StringTriple) {
        if self.parent.is_some() {
            self.removals.push(triple);
        }
    }

    fn remove_id_triple(&mut self, triple: IdTriple) {
        if self.parent.is_some() {
            self.id_removals.push(triple);
        }
    }

    fn commit(self) -> Box<dyn Future<Output = Result<(), std::io::Error>> + Send> {
        let parent = self.parent.clone();
        let mut additions: Vec<_> = match parent {
            None => self
                .additions
                .into_iter()
                .map(|triple| triple.to_unresolved())
                .collect(),
            Some(parent) => self
                .additions
                .into_par_iter()
                .map(move |triple| parent.string_triple_to_partially_resolved(triple))
                .collect(),
        };

        additions.extend(
            self.id_additions
                .into_iter()
                .map(|triple| triple.to_resolved()),
        );

        let parent = self.parent.clone();
        let mut removals: Vec<_>;
        if let Some(parent) = parent {
            removals = self
                .removals
                .into_par_iter()
                .filter_map(move |triple| {
                    parent
                        .string_triple_to_partially_resolved(triple)
                        .as_resolved()
                })
                .collect();

            removals.extend(self.id_removals.into_iter().map(|triple| triple));

            removals.par_sort_unstable();
            removals.dedup();
        } else {
            removals = Vec::with_capacity(0);
        }

        let (unresolved_nodes, (unresolved_predicates, unresolved_values)) = rayon::join(
            || {
                let unresolved_nodes_set: HashSet<_> = additions
                    .par_iter()
                    .filter_map(|triple| {
                        let subject = match triple.subject.is_resolved() {
                            true => None,
                            false => Some(triple.subject.as_ref().unwrap_unresolved().to_owned()),
                        };
                        let object = match triple.object.is_resolved() {
                            true => None,
                            false => match triple.object.as_ref().unwrap_unresolved() {
                                ObjectType::Node(node) => Some(node.to_owned()),
                                _ => None,
                            },
                        };

                        match (subject, object) {
                            (Some(subject), Some(object)) => Some(vec![subject, object]),
                            (Some(subject), _) => Some(vec![subject]),
                            (_, Some(object)) => Some(vec![object]),
                            _ => None,
                        }
                    })
                    .flatten()
                    .collect();

                let mut unresolved_nodes: Vec<_> = unresolved_nodes_set.into_iter().collect();
                unresolved_nodes.par_sort_unstable();

                unresolved_nodes
            },
            || {
                rayon::join(
                    || {
                        let unresolved_predicates_set: HashSet<_> = additions
                            .par_iter()
                            .filter_map(|triple| match triple.predicate.is_resolved() {
                                true => None,
                                false => {
                                    Some(triple.predicate.as_ref().unwrap_unresolved().to_owned())
                                }
                            })
                            .collect();
                        let mut unresolved_predicates: Vec<_> =
                            unresolved_predicates_set.into_iter().collect();
                        unresolved_predicates.par_sort_unstable();

                        unresolved_predicates
                    },
                    || {
                        let unresolved_values_set: HashSet<_> = additions
                            .par_iter()
                            .filter_map(|triple| match triple.object.is_resolved() {
                                true => None,
                                false => match triple.object.as_ref().unwrap_unresolved() {
                                    ObjectType::Value(value) => Some(value.to_owned()),
                                    _ => None,
                                },
                            })
                            .collect();
                        let mut unresolved_values: Vec<_> =
                            unresolved_values_set.into_iter().collect();
                        unresolved_values.par_sort_unstable();
                        unresolved_values
                    },
                )
            },
        );

        // store a copy. The original will be used to build the dictionaries.
        // The copy will be used later on to map unresolved strings to their id's before inserting
        let unresolved_nodes2 = unresolved_nodes.clone();
        let unresolved_predicates2 = unresolved_predicates.clone();
        let unresolved_values2 = unresolved_values.clone();
        match self.parent {
            Some(parent) => {
                let files = self.files.into_child();
                let builder = ChildLayerFileBuilder::from_files(parent.clone(), &files);

                Box::new(
                    builder
                        .add_nodes(unresolved_nodes)
                        .and_then(|(nodes, b)| {
                            b.add_predicates(unresolved_predicates)
                                .and_then(|(predicates, b)| {
                                    b.add_values(unresolved_values).and_then(|(values, b)| {
                                        b.into_phase2().map(move |b| (b, nodes, predicates, values))
                                    })
                                })
                        })
                        .and_then(move |(builder, node_ids, predicate_ids, value_ids)| {
                            let counts = parent.all_counts();
                            let parent_node_offset =
                                counts.node_count as u64 + counts.value_count as u64;
                            let parent_predicate_offset = counts.predicate_count as u64;
                            let mut node_map = HashMap::new();
                            for (node, id) in unresolved_nodes2.into_iter().zip(node_ids) {
                                node_map.insert(node, id + parent_node_offset);
                            }
                            let mut predicate_map = HashMap::new();
                            for (predicate, id) in
                                unresolved_predicates2.into_iter().zip(predicate_ids)
                            {
                                predicate_map.insert(predicate, id + parent_predicate_offset);
                            }
                            let mut value_map = HashMap::new();
                            for (value, id) in unresolved_values2.into_iter().zip(value_ids) {
                                value_map
                                    .insert(value, id + parent_node_offset + node_map.len() as u64);
                            }

                            let mut add_triples: Vec<_> = additions
                                .into_iter()
                                .map(|t| {
                                    t.resolve_with(&node_map, &predicate_map, &value_map)
                                        .expect("triple should have been resolvable")
                                })
                                .collect();
                            add_triples.par_sort_unstable();
                            add_triples.dedup();

                            builder
                                .add_id_triples(add_triples)
                                .and_then(move |b| b.remove_id_triples(removals))
                                .and_then(|b| b.finalize())
                        }),
                )
            }
            None => {
                let files = self.files.into_base();
                let builder = BaseLayerFileBuilder::from_files(&files);

                // TODO - this is exactly the same as above. We should generalize builder and run it once on the generalized instead.
                Box::new(
                    builder
                        .add_nodes(unresolved_nodes)
                        .and_then(|(nodes, b)| {
                            b.add_predicates(unresolved_predicates)
                                .and_then(|(predicates, b)| {
                                    b.add_values(unresolved_values).and_then(|(values, b)| {
                                        b.into_phase2().map(move |b| (b, nodes, predicates, values))
                                    })
                                })
                        })
                        .and_then(move |(builder, node_ids, predicate_ids, value_ids)| {
                            let mut node_map = HashMap::new();
                            for (node, id) in unresolved_nodes2.into_iter().zip(node_ids) {
                                node_map.insert(node, id);
                            }
                            let mut predicate_map = HashMap::new();
                            for (predicate, id) in
                                unresolved_predicates2.into_iter().zip(predicate_ids)
                            {
                                predicate_map.insert(predicate, id);
                            }
                            let mut value_map = HashMap::new();
                            for (value, id) in unresolved_values2.into_iter().zip(value_ids) {
                                value_map.insert(value, id + node_map.len() as u64);
                            }

                            let mut triples: Vec<_> = additions
                                .into_iter()
                                .map(|t| {
                                    t.resolve_with(&node_map, &predicate_map, &value_map)
                                        .expect("triple should have been resolvable")
                                })
                                .collect();
                            triples.par_sort_unstable();
                            triples.dedup();

                            builder.add_id_triples(triples).and_then(|b| b.finalize())
                        }),
                )
            }
        }
    }

    fn commit_boxed(self: Box<Self>) -> Box<dyn Future<Output = Result<(), std::io::Error>> + Send> {
        let builder = *self;
        builder.commit()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::internal::InternalLayer;
    use crate::storage::memory::*;
    use futures::sync::oneshot;
    use tokio::runtime::{Runtime, TaskExecutor};

    fn new_base_files() -> BaseLayerFiles<MemoryBackedStore> {
        BaseLayerFiles {
            node_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },
            predicate_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },
            value_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },

            subjects_file: MemoryBackedStore::new(),
            objects_file: MemoryBackedStore::new(),

            s_p_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            sp_o_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            o_ps_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            predicate_wavelet_tree_files: BitIndexFiles {
                bits_file: MemoryBackedStore::new(),
                blocks_file: MemoryBackedStore::new(),
                sblocks_file: MemoryBackedStore::new(),
            },
        }
    }

    fn new_child_files() -> ChildLayerFiles<MemoryBackedStore> {
        ChildLayerFiles {
            node_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },
            predicate_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },
            value_dictionary_files: DictionaryFiles {
                blocks_file: MemoryBackedStore::new(),
                offsets_file: MemoryBackedStore::new(),
            },

            pos_subjects_file: MemoryBackedStore::new(),
            pos_objects_file: MemoryBackedStore::new(),
            neg_subjects_file: MemoryBackedStore::new(),
            neg_objects_file: MemoryBackedStore::new(),

            pos_s_p_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            pos_sp_o_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            pos_o_ps_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            neg_s_p_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            neg_sp_o_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            neg_o_ps_adjacency_list_files: AdjacencyListFiles {
                bitindex_files: BitIndexFiles {
                    bits_file: MemoryBackedStore::new(),
                    blocks_file: MemoryBackedStore::new(),
                    sblocks_file: MemoryBackedStore::new(),
                },
                nums_file: MemoryBackedStore::new(),
            },
            pos_predicate_wavelet_tree_files: BitIndexFiles {
                bits_file: MemoryBackedStore::new(),
                blocks_file: MemoryBackedStore::new(),
                sblocks_file: MemoryBackedStore::new(),
            },
            neg_predicate_wavelet_tree_files: BitIndexFiles {
                bits_file: MemoryBackedStore::new(),
                blocks_file: MemoryBackedStore::new(),
                sblocks_file: MemoryBackedStore::new(),
            },
        }
    }

    fn example_base_layer(executor: &TaskExecutor) -> Arc<InternalLayer> {
        let name = [1, 2, 3, 4, 5];
        let files = new_base_files();
        let mut builder = SimpleLayerBuilder::new(name, files.clone());

        builder.add_string_triple(StringTriple::new_value("cow", "says", "moo"));
        builder.add_string_triple(StringTriple::new_value("pig", "says", "oink"));
        builder.add_string_triple(StringTriple::new_value("duck", "says", "quack"));

        oneshot::spawn(builder.commit(), executor).wait().unwrap();

        let layer = BaseLayer::load_from_files(name, &files).wait().unwrap();
        Arc::new(layer.into())
    }

    #[test]
    fn simple_base_layer_construction() {
        let runtime = Runtime::new().unwrap();
        let layer = example_base_layer(&runtime.executor());

        assert!(layer.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(layer.string_triple_exists(&StringTriple::new_value("pig", "says", "oink")));
        assert!(layer.string_triple_exists(&StringTriple::new_value("duck", "says", "quack")));
    }

    #[test]
    fn simple_child_layer_construction() {
        let runtime = Runtime::new().unwrap();
        let base_layer = example_base_layer(&runtime.executor());
        let files = new_child_files();
        let name = [0, 0, 0, 0, 0];
        let mut builder = SimpleLayerBuilder::from_parent(name, base_layer.clone(), files.clone());

        builder.add_string_triple(StringTriple::new_value("horse", "says", "neigh"));
        builder.add_string_triple(StringTriple::new_node("horse", "likes", "cow"));
        builder.remove_string_triple(StringTriple::new_value("duck", "says", "quack"));

        oneshot::spawn(builder.commit(), &runtime.executor())
            .wait()
            .unwrap();
        let child_layer = Arc::new(
            ChildLayer::load_from_files(name, base_layer, &files)
                .wait()
                .unwrap(),
        );

        assert!(
            child_layer.string_triple_exists(&StringTriple::new_value("horse", "says", "neigh"))
        );
        assert!(child_layer.string_triple_exists(&StringTriple::new_node("horse", "likes", "cow")));
        assert!(child_layer.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(child_layer.string_triple_exists(&StringTriple::new_value("pig", "says", "oink")));
        assert!(
            !child_layer.string_triple_exists(&StringTriple::new_value("duck", "says", "quack"))
        );
    }

    #[test]
    fn multi_level_layers() {
        let runtime = Runtime::new().unwrap();
        let base_layer = example_base_layer(&runtime.executor());
        let name2 = [0, 0, 0, 0, 0];
        let files2 = new_child_files();
        let mut builder =
            SimpleLayerBuilder::from_parent(name2, base_layer.clone(), files2.clone());

        builder.add_string_triple(StringTriple::new_value("horse", "says", "neigh"));
        builder.add_string_triple(StringTriple::new_node("horse", "likes", "cow"));
        builder.remove_string_triple(StringTriple::new_value("duck", "says", "quack"));

        oneshot::spawn(builder.commit(), &runtime.executor())
            .wait()
            .unwrap();
        let layer2: Arc<InternalLayer> = Arc::new(
            ChildLayer::load_from_files(name2, base_layer, &files2)
                .wait()
                .unwrap()
                .into(),
        );

        let name3 = [0, 0, 0, 0, 1];
        let files3 = new_child_files();
        builder = SimpleLayerBuilder::from_parent(name3, layer2.clone(), files3.clone());
        builder.remove_string_triple(StringTriple::new_node("horse", "likes", "cow"));
        builder.add_string_triple(StringTriple::new_node("horse", "likes", "pig"));
        builder.add_string_triple(StringTriple::new_value("duck", "says", "quack"));

        oneshot::spawn(builder.commit(), &runtime.executor())
            .wait()
            .unwrap();
        let layer3: Arc<InternalLayer> = Arc::new(
            ChildLayer::load_from_files(name3, layer2, &files3)
                .wait()
                .unwrap()
                .into(),
        );

        let name4 = [0, 0, 0, 0, 1];
        let files4 = new_child_files();
        builder = SimpleLayerBuilder::from_parent(name4, layer3.clone(), files4.clone());
        builder.remove_string_triple(StringTriple::new_value("pig", "says", "oink"));
        builder.add_string_triple(StringTriple::new_node("cow", "likes", "horse"));
        oneshot::spawn(builder.commit(), &runtime.executor())
            .wait()
            .unwrap();
        let layer4 = Arc::new(
            ChildLayer::load_from_files(name4, layer3, &files4)
                .wait()
                .unwrap(),
        );

        assert!(layer4.string_triple_exists(&StringTriple::new_value("cow", "says", "moo")));
        assert!(layer4.string_triple_exists(&StringTriple::new_value("duck", "says", "quack")));
        assert!(layer4.string_triple_exists(&StringTriple::new_value("horse", "says", "neigh")));
        assert!(layer4.string_triple_exists(&StringTriple::new_node("horse", "likes", "pig")));
        assert!(layer4.string_triple_exists(&StringTriple::new_node("cow", "likes", "horse")));

        assert!(!layer4.string_triple_exists(&StringTriple::new_value("pig", "says", "oink")));
        assert!(!layer4.string_triple_exists(&StringTriple::new_node("horse", "likes", "cow")));
    }
}
