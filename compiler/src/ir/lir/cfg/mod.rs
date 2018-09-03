use ::ir::SSAVariable;
use super::{ Phi, OpKind, Source, Op };
use ::petgraph::Graph;

mod builder;
pub use self::builder::FunctionCfgBuilder;

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct LabelN(pub ::petgraph::graph::NodeIndex);
impl ::std::fmt::Display for LabelN {
    fn fmt(&self, f: &mut ::std::fmt::Formatter) -> ::std::fmt::Result {
        write!(f, "L{}", self.0.index())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct EdgeN(pub ::petgraph::graph::EdgeIndex);

#[derive(Debug)]
pub struct BasicBlock {
    pub label: Option<LabelN>,
    pub phi_nodes: Vec<Phi>,
    pub ops: Vec<Op>,
    pub outgoing_edges: Vec<EdgeN>,
}

#[derive(Debug)]
pub struct FunctionCfg {
    pub entry: LabelN,
    pub cfg: Graph<BasicBlock, BasicBlockEdge>,
}

#[derive(Debug)]
pub struct BasicBlockEdge {
    writes: Vec<SSAVariable>,
}

impl FunctionCfg {

    pub fn new() -> Self {
        let mut cfg = Graph::new();

        let entry = cfg.add_node(BasicBlock {
            label: None,
            phi_nodes: vec![],
            ops: vec![],
            outgoing_edges: vec![],
        });
        cfg[entry].label = Some(LabelN(entry));

        FunctionCfg {
            entry: LabelN(entry),
            cfg: cfg,
        }
    }

    pub fn blocks_iter<'a>(&'a self) -> Box<Iterator<Item = &BasicBlock> + 'a> {
        Box::new(self.cfg.raw_nodes().iter()
                 .map(|n| &n.weight))
    }

    pub fn blocks_iter_mut<'a>(&'a mut self) -> Box<Iterator<Item = &mut BasicBlock> + 'a> {
        Box::new(self.cfg.node_weights_mut())
    }

    pub fn labels_iter(&self) -> Box<Iterator<Item = LabelN>> {
        Box::new(self.cfg.node_indices().map(|i| LabelN(i)))
    }

    pub fn entry(&self) -> LabelN {
        self.entry
    }

    pub fn block<'a>(&'a self, lbl: LabelN) -> &BasicBlock {
        self.cfg.node_weight(lbl.0).unwrap()
    }

    pub fn jumps_iter<'a>(&'a self, lbl: LabelN) -> impl Iterator<Item = EdgeN> + 'a {
        let node = self.cfg.node_weight(lbl.0).unwrap();
        node.outgoing_edges.iter().map(|e| *e)
    }

    pub fn branch_slots(&self, lbl: LabelN) -> Vec<LabelN> {
        self.jumps_iter(lbl)
            .map(|edge| self.edge_target(edge))
            .collect()
    }

    pub fn edge_target(&self, lbl: EdgeN) -> LabelN {
        LabelN(self.cfg.edge_endpoints(lbl.0).unwrap().1)
    }

}