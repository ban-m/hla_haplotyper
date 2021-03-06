// TODO: Use copy number estimation module to determine how many clusteres are there, or just ...
use definitions::DataSet;
use rand::{Rng, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use serde::*;
use std::collections::{HashMap, HashSet};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MultiplicityEstimationConfig {
    max_cluster: usize,
    seed: u64,
    path: Option<String>,
    thread: usize,
}

impl MultiplicityEstimationConfig {
    pub fn new(thread: usize, max_cluster: usize, seed: u64, path: Option<&str>) -> Self {
        Self {
            thread,
            max_cluster,
            seed,
            path: path.map(|x| x.to_string()),
        }
    }
}

pub trait MultiplicityEstimation {
    fn estimate_multiplicity(self, config: &MultiplicityEstimationConfig) -> Self;
}

impl MultiplicityEstimation for DataSet {
    fn estimate_multiplicity(mut self, config: &MultiplicityEstimationConfig) -> Self {
        for read in self.encoded_reads.iter_mut() {
            for node in read.nodes.iter_mut() {
                node.cluster = 0;
            }
        }
        self.assignments = self
            .encoded_reads
            .iter()
            .map(|r| definitions::Assignment::new(r.id, 0))
            .collect();
        use super::Assemble;
        let assemble_config = super::AssembleConfig::new(config.thread, 100, false);
        debug!("Start assembling {} reads", self.encoded_reads.len());
        let graphs = self.assemble_as_graph(&assemble_config);
        debug!("Assembled reads.");
        if let Some(mut file) = config
            .path
            .as_ref()
            .and_then(|path| std::fs::File::create(path).ok())
            .map(std::io::BufWriter::new)
        {
            use std::io::Write;
            let gfa = self.assemble_as_gfa(&assemble_config);
            writeln!(&mut file, "{}", gfa).unwrap();
        }
        debug!("GRAPH\tID\tCoverage\tMean\tLen");
        let mut single_copy_coverage = 0f64;
        let estimated_cluster_num: HashMap<u64, usize> = graphs
            .iter()
            .map(|graph| estimate_graph_multiplicity(&self, graph, config))
            .fold(HashMap::new(), |mut x, (result, cov)| {
                single_copy_coverage += cov;
                for (unit, cluster) in result {
                    x.insert(unit, cluster);
                }
                x
            });
        single_copy_coverage /= graphs.len() as f64;
        for unit in self.selected_chunks.iter_mut() {
            if let Some(&cl_num) = estimated_cluster_num.get(&unit.id) {
                unit.cluster_num = cl_num;
            }
        }
        self.coverage = Some(single_copy_coverage);
        self
    }
    // fn estimate_multiplicity_new(mut self, config: &MultiplicityEstimationConfig) -> Self {
    //     let mut counts: HashMap<_, u64> = HashMap::new();
    //     for node in self.encoded_reads.iter().flat_map(|e| e.nodes.iter()) {
    //         *counts.entry(node.unit).or_default() += 1;
    //     }
    //     let (_, single_copy_coverage) = cluster_coverage(&counts, config);
    //     use crate::assemble::ditch_graph::DitchGraph;
    //     use crate::assemble::AssembleConfig;
    //     let reads: Vec<_> = self.encoded_reads.iter().collect();
    //     assert!(reads
    //         .iter()
    //         .flat_map(|r| r.nodes.iter())
    //         .all(|n| n.cluster == 0));
    //     let c = AssembleConfig::new(config.thread, 1000, false);
    //     let mut graph = DitchGraph::new(&reads, Some(&self.selected_chunks), &c);
    //     graph.remove_lightweight_edges(1);
    //     let lens: Vec<_> = self.raw_reads.iter().map(|x| x.seq().len()).collect();
    //     let (node_copy_number, _) = graph.copy_number_estimation(single_copy_coverage, &lens);
    //     debug!("COPYTNUM\tID\tCov\tCopy");
    //     for unit in self.selected_chunks.iter_mut() {
    //         let copy_number = node_copy_number[&(unit.id, 0)];
    //         unit.cluster_num = copy_number;
    //         debug!(
    //             "COPYNUM\t{}\t{}\t{}",
    //             unit.id, counts[&unit.id], copy_number
    //         );
    //     }
    //     self
    // }
}

fn estimate_graph_multiplicity(
    ds: &DataSet,
    graph: &super::assemble::Graph,
    c: &MultiplicityEstimationConfig,
) -> (Vec<(u64, usize)>, f64) {
    let covs: Vec<_> = graph
        .nodes
        .iter()
        .map(|node| {
            let len = node.segments.len();
            let unit: HashSet<_> = node.segments.iter().map(|t| t.unit).collect();
            let coverage = ds
                .encoded_reads
                .iter()
                .map(|r| r.nodes.iter().filter(|n| unit.contains(&n.unit)).count())
                .sum::<usize>();
            let mean = (coverage / len) as u64;
            mean
        })
        .collect();
    use rayon::prelude::*;
    let (model, aic): (Model, f64) = (1..c.max_cluster)
        .into_par_iter()
        .map(|k| {
            let seed = k as u64 + c.seed;
            let (model, lk) = clustering(&covs, k, seed);
            // Lambda for each cluster, fraction for each cluster,
            // and one constraint that the sum of the fractions equals to 1.
            let aic = -2. * lk + (2. * k as f64 - 1.);
            (model, aic)
        })
        .min_by(|x, y| (x.1).partial_cmp(&y.1).unwrap())
        .unwrap();
    debug!("AIC\t{}", aic);
    let assignments: Vec<_> = covs.iter().map(|&d| model.assign(d)).collect();
    let single_copy_coverage = {
        let min = model
            .lambdas
            .iter()
            .min_by(|a, b| a.partial_cmp(&b).unwrap())
            .unwrap();
        let coverage = model
            .lambdas
            .iter()
            .filter(|&x| (x - min).abs() > 0.01)
            .min_by(|a, b| a.partial_cmp(&b).unwrap())
            .unwrap_or(min);
        debug!("DIPLOID\t{}", coverage);
        coverage / 2.
    };
    let repeat_num: Vec<_> = model
        .lambdas
        .iter()
        .map(|x| ((x / single_copy_coverage) + 0.5).floor() as usize)
        .collect();
    debug!("LAMBDAS:{:?}", model.lambdas);
    debug!("PREDCT:{:?}", repeat_num);
    let mut result = vec![];
    debug!("REPEATNUM\tID\tMULTP\tCLUSTER");
    for (&cl, contig) in assignments.iter().zip(graph.nodes.iter()) {
        let repeat_num = repeat_num[cl];
        debug!("REPEATNUM\t{}\t{}\t{}", contig.id, repeat_num, cl);
        for node in contig.segments.iter() {
            result.push((node.unit, repeat_num));
        }
    }
    (result, single_copy_coverage)
}

struct Model {
    cluster: usize,
    fractions: Vec<f64>,
    lambdas: Vec<f64>,
}
const SMALL: f64 = 0.00000000000000001;
impl Model {
    fn new(data: &[u64], weight: &[Vec<f64>], k: usize) -> Self {
        let sum: Vec<_> = (0..k)
            .map(|cl| weight.iter().map(|ws| ws[cl]).sum::<f64>() + SMALL)
            .collect();
        let fractions: Vec<_> = sum.iter().map(|w| w / data.len() as f64).collect();
        let lambdas: Vec<_> = sum
            .iter()
            .enumerate()
            .map(|(cl, sum)| {
                weight
                    .iter()
                    .zip(data)
                    .map(|(ws, &x)| x as f64 * ws[cl])
                    .sum::<f64>()
                    / sum
            })
            .collect();
        let cluster = k;
        Self {
            cluster,
            fractions,
            lambdas,
        }
    }
    fn lk(&self, data: &[u64]) -> f64 {
        data.iter().map(|&d| self.lk_data(d)).sum::<f64>()
    }
    fn lk_data(&self, data: u64) -> f64 {
        let lks: Vec<_> = (0..self.cluster)
            .map(|cl| {
                self.fractions[cl].ln() + data as f64 * self.lambdas[cl].ln()
                    - self.lambdas[cl]
                    - (0..data).map(|x| ((x + 1) as f64).ln()).sum::<f64>()
            })
            .collect();
        logsumexp(&lks)
    }
    fn update_weight(&self, ws: &mut [f64], data: u64) {
        assert_eq!(ws.len(), self.cluster);
        for (cl, w) in ws.iter_mut().enumerate() {
            *w = self.fractions[cl].ln() + data as f64 * self.lambdas[cl].ln()
                - self.lambdas[cl]
                - (0..data).map(|x| ((x + 1) as f64).ln()).sum::<f64>();
        }
        let lk = logsumexp(&ws);
        ws.iter_mut().for_each(|x| *x = (*x - lk).exp());
    }
    // fn new_weight(&self, data: u64) -> Vec<f64> {
    //     let lks: Vec<_> = (0..self.cluster)
    //         .map(|cl| {
    //             self.fractions[cl].ln() + data as f64 * self.lambdas[cl].ln()
    //                 - self.lambdas[cl]
    //                 - (0..data).map(|x| ((x + 1) as f64).ln()).sum::<f64>()
    //         })
    //         .collect();
    //     let lk = logsumexp(&lks);
    //     assert!((1. - lks.iter().map(|x| (x - lk).exp()).sum::<f64>()).abs() < 0.0001);
    //     lks.iter().map(|x| (x - lk).exp()).collect()
    // }
    fn assign(&self, data: u64) -> usize {
        let (cl, _) = (0..self.cluster)
            .map(|cl| {
                self.fractions[cl].ln() + data as f64 * self.lambdas[cl].ln()
                    - self.lambdas[cl]
                    - (0..data).map(|x| ((x + 1) as f64).ln()).sum::<f64>()
            })
            .enumerate()
            .max_by(|x, y| (x.1).partial_cmp(&y.1).unwrap())
            .unwrap();
        cl
    }
}

fn logsumexp(xs: &[f64]) -> f64 {
    if xs.is_empty() {
        return 0.;
    }
    let max = xs.iter().max_by(|x, y| x.partial_cmp(&y).unwrap()).unwrap();
    let sum = xs.iter().map(|x| (x - max).exp()).sum::<f64>().ln();
    assert!(sum >= 0., "{:?}->{}", xs, sum);
    max + sum
}

fn clustering(data: &[u64], k: usize, seed: u64) -> (Model, f64) {
    let mut rng: Xoshiro256PlusPlus = SeedableRng::seed_from_u64(seed);
    let (weight, lk) = (0..5)
        .map(|_| {
            let mut weight: Vec<_> = (0..data.len())
                .map(|_| {
                    let mut ws = vec![0.; k];
                    ws[rng.gen::<usize>() % k] = 1.;
                    ws
                })
                .collect();
            let mut lk = std::f64::NEG_INFINITY;
            loop {
                let model = Model::new(data, &weight, k);
                let new_lk = model.lk(data);
                let diff = new_lk - lk;
                if diff < 0.00001 {
                    break;
                }
                lk = new_lk;
                for (ws, d) in weight.iter_mut().zip(data.iter()) {
                    model.update_weight(ws, *d);
                }
            }
            (weight, lk)
        })
        .max_by(|x, y| (x.1).partial_cmp(&y.1).unwrap())
        .unwrap();
    let model = Model::new(data, &weight, k);
    (model, lk)
}

// pub fn cluster_coverage(
//     unit_covs: &HashMap<u64, u64>,
//     c: &MultiplicityEstimationConfig,
// ) -> (Vec<(u64, usize)>, f64) {
//     use rayon::prelude::*;
//     let covs: Vec<_> = unit_covs.values().copied().collect();
//     let (model, aic): (Model, f64) = (1..c.max_cluster)
//         .into_par_iter()
//         .map(|k| {
//             let seed = k as u64 + c.seed;
//             let (model, lk) = clustering(&covs, k, seed);
//             let aic = -2. * lk + (2. * k as f64 - 1.);
//             (model, aic)
//         })
//         .min_by(|x, y| (x.1).partial_cmp(&y.1).unwrap())
//         .unwrap();
//     debug!("AIC\t{}", aic);
//     let assignments: Vec<_> = covs.iter().map(|&d| model.assign(d)).collect();
//     let single_copy_coverage = {
//         let min = model
//             .lambdas
//             .iter()
//             .min_by(|a, b| a.partial_cmp(&b).unwrap())
//             .unwrap();
//         let coverage = model
//             .lambdas
//             .iter()
//             .filter(|&x| (x - min).abs() > 0.01)
//             .min_by(|a, b| a.partial_cmp(&b).unwrap())
//             .unwrap_or(min);
//         debug!("DIPLOID\t{}", coverage);
//         coverage / 2.
//     };
//     let repeat_num: Vec<_> = model
//         .lambdas
//         .iter()
//         .map(|x| ((x / single_copy_coverage) + 0.5).floor() as usize)
//         .collect();
//     debug!("LAMBDAS:{:?}", model.lambdas);
//     debug!("PREDCT:{:?}", repeat_num);
//     let mut result = vec![];
//     for (&cl, (&unit, &_)) in assignments.iter().zip(unit_covs.iter()) {
//         let repeat_num = repeat_num[cl];
//         result.push((unit, repeat_num));
//     }
//     (result, single_copy_coverage)
// }
