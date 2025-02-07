use super::{cluster, liblinear, Model, Tree, TreeNode};
use crate::data::DataSet;
use crate::mat_util::*;
use crate::util::{create_progress_bar, ProgressBar};
use crate::{Index, IndexSet, IndexValueVec};
use hashbrown::HashMap;
use itertools::{izip, Itertools};
use log::info;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::iter::FromIterator;
use std::sync::{Arc, Mutex};

/// Model training hyper-parameters.
#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub struct HyperParam {
    pub n_trees: usize,
    pub min_branch_size: usize,
    pub max_depth: usize,
    pub centroid_threshold: f32,
    pub linear: liblinear::HyperParam,
    pub cluster: cluster::HyperParam,
}

impl Default for HyperParam {
    fn default() -> Self {
        Self {
            n_trees: 3,
            min_branch_size: 100,
            max_depth: 20,
            centroid_threshold: 0.,
            linear: liblinear::HyperParam::default(),
            cluster: cluster::HyperParam::default(),
        }
    }
}

impl HyperParam {
    /// Check if the hyper-parameter settings are valid.
    pub fn validate(&self) -> Result<(), String> {
        if self.n_trees == 0 {
            Err(format!("n_trees must be positive, but is {}", self.n_trees))
        } else if self.min_branch_size <= 1 {
            Err(format!(
                "min_branch_size must be greater than 1, but is {}",
                self.min_branch_size
            ))
        } else if self.centroid_threshold < 0. {
            Err(format!(
                "centroid_threshold must be non-negative, but is {}",
                self.centroid_threshold
            ))
        } else if self.max_depth == 0 {
            Err(format!(
                "max_depth must be positive, but is {}",
                self.max_depth
            ))
        } else if let Err(msg) = self.linear.validate() {
            Err(format!("Invalid liblinear hyper-parameter; {}", msg))
        } else if let Err(msg) = self.cluster.validate() {
            Err(format!("Invalid clustering hyper-parameter; {}", msg))
        } else {
            Ok(())
        }
    }

    /// Train a parabel model on the given dataset.
    ///
    /// Here we take ownership of the dataset object to perform necessary prepossessing. One can
    /// choose to clone a dataset before passing it in to avoid losing the original data.
    pub fn train(&self, dataset: DataSet) -> Model {
        self.validate().unwrap();
        let n_features = dataset.n_features;

        info!("Training Parabel model with hyper-parameters {:?}", self);
        let start_t = time::precise_time_s();

        info!("Initializing tree trainer");
        let trainer = TreeTrainer::initialize(dataset, *self);

        info!("Start training forest");
        let trees: Vec<_> = (0..self.n_trees)
            .into_par_iter()
            .map(|_| trainer.train())
            .collect();

        info!(
            "Parabel model training complete; it took {:.2}s",
            time::precise_time_s() - start_t
        );
        Model { trees, n_features }
    }
}

struct TreeTrainer {
    all_examples: Arc<TrainingExamples>,
    all_labels: Arc<LabelCluster>,
    hyper_param: HyperParam,
    progress_bar: Mutex<ProgressBar>,
}

impl TreeTrainer {
    /// Initialize a reusable tree trainer with the dataset and hyper-parameters.
    ///
    /// Dataset is assumed to be well-formed.
    fn initialize(mut dataset: DataSet, hyper_param: HyperParam) -> Self {
        assert_eq!(dataset.feature_lists.len(), dataset.label_sets.len());
        // l2-normalize all examples in the dataset
        dataset
            .feature_lists
            .par_iter_mut()
            .for_each(|v| v.l2_normalize());
        // Initialize label clusters
        let all_labels = Arc::new(LabelCluster::new_from_dataset(
            &dataset,
            hyper_param.centroid_threshold,
        ));

        // Initialize examples set
        let all_examples = Arc::new(TrainingExamples::new_from_dataset(dataset));

        let progress_bar = Mutex::new(create_progress_bar(
            (all_labels.len() * hyper_param.n_trees) as u64,
        ));

        Self {
            all_examples,
            all_labels,
            hyper_param,
            progress_bar,
        }
    }

    #[inline]
    fn classifier_hyper_param(&self, n_examples: usize) -> liblinear::HyperParam {
        self.hyper_param
            .linear
            .adapt_to_sample_size(n_examples, self.all_examples.len())
    }

    fn train(&self) -> Tree {
        Tree {
            root: self.train_subtree(1, self.all_examples.clone(), self.all_labels.clone()),
        }
    }

    fn train_subtree(
        &self,
        depth: usize,
        examples: Arc<TrainingExamples>,
        label_cluster: Arc<LabelCluster>,
    ) -> TreeNode {
        // If we haven't reached depth limit, have enough labels for further branching,
        // and also successfully performed clustering, then recursively branch and train subtrees
        if depth < self.hyper_param.max_depth
            && label_cluster.len() >= self.hyper_param.min_branch_size
        {
            if let Some(label_clusters) = label_cluster.split(self.hyper_param.cluster) {
                assert!(label_clusters.len() > 1);
                self.progress_bar
                    .lock()
                    .expect("Failed to lock progress bar")
                    .total += label_clusters.len() as u64;

                drop(label_cluster); // No longer needed

                let example_index_lists = label_clusters
                    .par_iter()
                    .map(|cluster| examples.find_examples_with_labels(&cluster.labels))
                    .collect::<Vec<_>>();

                let (children, classifier) = rayon::join(
                    {
                        let examples = examples.clone();
                        || {
                            self.train_child_nodes(
                                depth,
                                examples,
                                label_clusters,
                                &example_index_lists,
                            )
                        }
                    },
                    || {
                        self.train_classifier(
                            examples, // NB: the Arc "examples" is moved into this closure
                            &example_index_lists,
                        )
                    },
                );

                return TreeNode::BranchNode {
                    classifier,
                    children,
                };
            }
        }

        // Otherwise stop branching and train a leaf node
        self.train_leaf_node(examples, &label_cluster.labels)
    }

    fn train_child_nodes(
        &self,
        depth: usize,
        examples: Arc<TrainingExamples>,
        label_clusters: Vec<LabelCluster>,
        example_index_lists: &[Vec<usize>],
    ) -> Vec<TreeNode> {
        // NB: the examples arc itself is moved when creating this vector of clones
        let example_arcs = vec![examples; label_clusters.len()];
        label_clusters
            .into_par_iter()
            .zip_eq(example_index_lists.par_iter())
            .zip_eq(example_arcs.into_par_iter())
            .map(|((label_cluster, example_indices), examples)| {
                let cluster_examples = examples.take_examples_by_indices(example_indices);
                drop(examples); // No longer needed
                self.train_subtree(
                    depth + 1,
                    Arc::new(cluster_examples),
                    Arc::new(label_cluster),
                )
            })
            .collect()
    }

    fn train_leaf_node(&self, examples: Arc<TrainingExamples>, leaf_labels: &[Index]) -> TreeNode {
        let classifier = {
            let example_index_lists = leaf_labels
                .par_iter()
                .map(|&label| examples.find_examples_with_label(label))
                .collect::<Vec<_>>();
            self.train_classifier(examples, &example_index_lists)
        };
        TreeNode::LeafNode {
            classifier,
            labels: leaf_labels.to_vec(),
        }
    }

    fn train_classifier(
        &self,
        examples: Arc<TrainingExamples>,
        label_to_example_indices: &[Vec<usize>],
    ) -> liblinear::MultiLabelClassifier {
        let classifier = self.classifier_hyper_param(examples.len()).train(
            &examples.feature_matrix.view(),
            label_to_example_indices,
            &examples.index_to_feature,
            self.all_examples.n_features(),
        );

        self.progress_bar
            .lock()
            .expect("Failed to lock progress bar")
            .add(label_to_example_indices.len() as u64);

        classifier
    }
}

/// Internal representation of training examples for training a subtree.
struct TrainingExamples {
    feature_matrix: SparseMat,
    index_to_feature: Vec<Index>,
    label_sets: Vec<Arc<IndexSet>>,
}

impl TrainingExamples {
    #[inline]
    fn new(
        feature_matrix: SparseMat,
        index_to_feature: Vec<Index>,
        label_sets: Vec<Arc<IndexSet>>,
    ) -> Self {
        assert_eq!(feature_matrix.rows(), label_sets.len());
        assert!(!label_sets.is_empty());
        assert_eq!(feature_matrix.cols(), index_to_feature.len());
        Self {
            feature_matrix,
            index_to_feature,
            label_sets,
        }
    }

    fn new_from_dataset(dataset: DataSet) -> Self {
        let DataSet {
            n_features,
            n_labels: _,
            mut feature_lists,
            label_sets,
        } = dataset;

        // Append bias term to each vector to make training linear classifiers easier
        let bias_index = n_features as Index;
        feature_lists
            .iter_mut()
            .for_each(|v| v.push((bias_index, 1.)));

        let feature_matrix = csrmat_from_index_value_pair_lists(
            feature_lists,
            n_features + 1, // + 1 because we added bias term
        );
        let index_to_feature = (0..feature_matrix.cols() as Index).collect_vec();
        let label_sets = label_sets.into_iter().map(Arc::new).collect_vec();

        Self::new(feature_matrix, index_to_feature, label_sets)
    }

    #[inline]
    fn len(&self) -> usize {
        self.feature_matrix.rows()
    }

    #[inline]
    fn n_features(&self) -> usize {
        self.feature_matrix.cols()
    }

    fn find_examples_with_label(&self, label: Index) -> Vec<usize> {
        self.label_sets
            .par_iter()
            .enumerate()
            .filter_map(|(i, example_labels)| {
                if example_labels.contains(&label) {
                    Some(i)
                } else {
                    None
                }
            })
            .collect()
    }

    fn find_examples_with_labels(&self, labels: &[Index]) -> Vec<usize> {
        let labels = IndexSet::from_iter(labels.iter().cloned());
        self.label_sets
            .par_iter()
            .enumerate()
            .filter_map(|(i, example_labels)| {
                if example_labels.is_disjoint(&labels) {
                    None
                } else {
                    Some(i)
                }
            })
            .collect()
    }

    fn take_examples_by_indices(&self, indices: &[usize]) -> Self {
        let (new_feature_matrix, mut new_index_to_feature) = self
            .feature_matrix
            .copy_outer_dims(&indices)
            .shrink_column_indices();
        for index in &mut new_index_to_feature {
            *index = self.index_to_feature[*index as usize];
        }

        let new_label_sets = indices
            .iter()
            .map(|&i| self.label_sets[i].clone())
            .collect_vec();
        Self::new(new_feature_matrix, new_index_to_feature, new_label_sets)
    }
}

/// Internal representation of label cluster for building the structure of a subtree.
struct LabelCluster {
    labels: Vec<Index>,
    feature_matrix: SparseMat,
}

impl LabelCluster {
    fn new(labels: Vec<Index>, feature_matrix: SparseMat) -> Self {
        assert_eq!(labels.len(), feature_matrix.rows());
        assert!(!labels.is_empty());
        Self {
            labels,
            feature_matrix,
        }
    }

    fn new_from_dataset(dataset: &DataSet, centroid_threshold: f32) -> Self {
        let (labels, label_centroids) = Self::compute_label_centroids(&dataset, centroid_threshold);
        let label_centroids =
            csrmat_from_index_value_pair_lists(label_centroids, dataset.n_features);
        Self::new(labels, label_centroids)
    }

    /// Compute centroid feature vectors for labels in a given dataset, pruned with the given threshold.
    ///
    /// Assumes that dataset is well-formed.
    fn compute_label_centroids(
        dataset: &DataSet,
        threshold: f32,
    ) -> (Vec<Index>, Vec<IndexValueVec>) {
        let mut label_to_feature_to_sum =
            HashMap::<Index, HashMap<Index, f32>>::with_capacity(dataset.n_labels);
        for (features, labels) in izip!(&dataset.feature_lists, &dataset.label_sets) {
            for &label in labels {
                let feature_to_sum = label_to_feature_to_sum.entry(label).or_default();
                for &(feature, value) in features {
                    *feature_to_sum.entry(feature).or_default() += value;
                }
            }
        }

        label_to_feature_to_sum
            .into_iter()
            .map(|(label, feature_to_sum)| {
                let mut v = feature_to_sum.into_iter().collect_vec();
                v.l2_normalize();
                v.prune_with_threshold(threshold);
                v.sort_by_index();
                (label, v)
            })
            .unzip()
    }

    fn take_labels_by_indices(&self, indices: &[usize]) -> Self {
        let new_labels = indices.iter().map(|&i| self.labels[i]).collect_vec();
        let (new_feature_matrix, _) = self
            .feature_matrix
            .copy_outer_dims(indices)
            .shrink_column_indices();

        Self::new(new_labels, new_feature_matrix)
    }

    #[inline]
    fn len(&self) -> usize {
        self.feature_matrix.rows()
    }

    fn split(&self, hyper_param: cluster::HyperParam) -> Option<Vec<Self>> {
        let clusters = hyper_param.train(&self.feature_matrix.view());
        if clusters.len() > 1 {
            Some(
                clusters
                    .iter()
                    .map(|labels| self.take_labels_by_indices(labels))
                    .collect_vec(),
            )
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::iter::FromIterator;

    #[test]
    fn test_compute_label_centroids() {
        let dataset = DataSet {
            n_features: 4,
            n_labels: 3,
            feature_lists: vec![
                vec![(0, 1.), (2, 2.)],
                vec![(1, 1.), (3, 2.)],
                vec![(0, 1.), (3, 2.)],
            ],
            label_sets: vec![
                IndexSet::from_iter(vec![0, 1]),
                IndexSet::from_iter(vec![0, 2]),
                IndexSet::from_iter(vec![1, 2]),
            ],
        };

        let (labels, vecs) =
            LabelCluster::compute_label_centroids(&dataset, 1. / 18f32.sqrt() + 1e-4);
        assert_eq!(
            HashMap::<Index, IndexValueVec>::from_iter(
                vec![
                    (
                        0,
                        vec![
                            (0, 1. / 10f32.sqrt()),
                            (1, 1. / 10f32.sqrt()),
                            (2, 2. / 10f32.sqrt()),
                            (3, 2. / 10f32.sqrt()),
                        ]
                    ),
                    (
                        1,
                        vec![
                            (0, 2. / 12f32.sqrt()),
                            (2, 2. / 12f32.sqrt()),
                            (3, 2. / 12f32.sqrt()),
                        ]
                    ),
                    (
                        2,
                        vec![
                            // The first two entries are pruned by the given threshold
                            // (0, 1. / 18f32.sqrt()),
                            // (1, 1. / 18f32.sqrt()),
                            (3, 4. / 18f32.sqrt()),
                        ]
                    ),
                ]
                .into_iter()
            ),
            HashMap::<Index, IndexValueVec>::from_iter(labels.into_iter().zip(vecs.into_iter()))
        );
    }
}
