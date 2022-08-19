use crate::graph::{Graph, GraphId, Graphs, Node, NodeId, NodeKind};
use rustc_index::vec::IndexVec;
use rustc_middle::mir::Field;
use std::collections::HashMap;
use std::collections::HashSet;
use std::cmp;
use itertools::Itertools;
use std::fmt::{self, Debug, Display, Formatter};

/// The information checked in this struct is whether nodes flow to loads, stores, and offsets (pos
/// and neg), as well as whether they are unique.
/// Uniqueness of node X is determined based on whether there is a node Y which is X's ancestor
/// through copies, fields, and offsets, also is a node Z's ancestor through copies, fields, offsets,
/// where the fields are the same between X and Z, and where Z is chronologically between X and X's last descendent.
/// If such a node exists, X is not unique.
#[derive(Debug, Eq, PartialEq, Hash, Clone)]
pub struct NodeInfo {
    flows_to_mutation: Option<NodeId>,
    flows_to_load: Option<NodeId>,
    flows_to_pos_offset: Option<NodeId>,
    flows_to_neg_offset: Option<NodeId>,
    non_unique: Option<NodeId>,
}

//Algorithm for determining non-conflicting:
//First calculate all of the below from the bottom-up.
//Then create a tree where children are based on fields only,
//each node having a list of NodeIds associated with it.
//Finally, check that each node's starts and ends don't overlap with each other.
#[derive(Debug, Hash, Clone, Copy)]
struct GraphTraverseInfo {
    last_descendent: NodeId,
    flows_to_load: Option<NodeId>,
    flows_to_store: Option<NodeId>,
    flows_to_pos_offset: Option<NodeId>,
    flows_to_neg_offset: Option<NodeId>,
}

fn init_traverse_info (n_id: NodeId, n: &Node) -> GraphTraverseInfo {
    GraphTraverseInfo {
        last_descendent: n_id,
        flows_to_store: node_does_mutation(n).then(|| n_id),
        flows_to_load: node_does_load(n).then(|| n_id),
        flows_to_pos_offset: node_does_pos_offset(n).then(|| n_id),
        flows_to_neg_offset: node_does_neg_offset(n).then(|| n_id)
    }
}

fn create_flow_info (g: &Graph) -> HashMap<NodeId,GraphTraverseInfo> {
    let mut f = HashMap::from_iter(
        g.nodes.iter_enumerated()
        .map(|(idx,node)| (idx,init_traverse_info(idx,node)))
    );
    for (n_id,node) in g.nodes.iter_enumerated().rev(){ 
        let cur : GraphTraverseInfo = *(f.get(&n_id).unwrap());
        if let Some(p_id) = node.source {
            let parent = f.get_mut(&p_id).unwrap();
            parent.last_descendent = cmp::max(cur.last_descendent,parent.last_descendent);
            parent.flows_to_load = parent.flows_to_load.or(cur.flows_to_load);
            parent.flows_to_store = parent.flows_to_store.or(cur.flows_to_store);
            parent.flows_to_pos_offset = parent.flows_to_pos_offset.or(cur.flows_to_pos_offset);
            parent.flows_to_neg_offset = parent.flows_to_neg_offset.or(cur.flows_to_neg_offset);
        }
    }
    f
}

fn collect_children (g: &Graph) -> HashMap<NodeId,Vec<NodeId>> {
    let mut m = HashMap::new();
    for (par,chi) in g.nodes.iter_enumerated().filter_map(|(idx,node)| Some((node.source?,idx))){
        m.entry(par).or_insert_with(Vec::new).push(chi)
    };
    for (par,chi) in g.nodes.iter_enumerated(){
        m.try_insert(par,Vec::new());
    };
    m
}

fn partition_into_alias_sets (g: &Graph) -> HashMap<(NodeId,Vec<Field>),Vec<NodeId>> {
    let mut store_seen : HashMap<NodeId,(NodeId,Vec<Field>)> = HashMap::new();
    let mut store_roots : HashMap<(NodeId,Vec<Field>),Vec<NodeId>> = HashMap::new();

    for n_id in g.nodes.indices(){
        let node = g.nodes.get(n_id).unwrap();
        match node.source {
            None => {
                store_roots.insert((n_id,Vec::new()),vec![n_id]);
                store_seen.insert(n_id,(n_id,Vec::new()))
            },
            Some(par_id) => {
                let (parent_root,parent_fields) = store_seen.get(&par_id).unwrap().clone();
                match node.kind {
                    NodeKind::Field(f) => {
                        let mut cp = parent_fields;
                        cp.push(f);
                        store_roots.entry((parent_root,cp.clone())).or_insert_with(Vec::new).push(n_id);
                        store_seen.insert(n_id,(parent_root, cp))
                    },
                    _ => store_seen.insert(n_id,(parent_root, parent_fields)),
                }
            }
        };
    };
    store_roots
}

fn check_sibling_conflict(siblings: &mut Vec<NodeId>, flow_info: &HashMap<NodeId,GraphTraverseInfo>, conflict_result: &mut HashMap<NodeId,NodeId>){
    let mut max_desc : NodeId = *siblings.get(0).unwrap();
    let mut max_desc_parent : NodeId = *siblings.get(0).unwrap();
    for id in siblings {
        if *id < max_desc {
            conflict_result.insert(max_desc_parent,*id);
            conflict_result.insert(*id,max_desc_parent);
        }
        if flow_info.get(&id).unwrap().last_descendent > max_desc {
            max_desc = flow_info.get(&id).unwrap().last_descendent;
            max_desc_parent = *id
        }
    }
}

fn determine_non_conflicting(g: &Graph, downward: &HashMap<NodeId,Vec<NodeId>>, flow_info: &HashMap<NodeId,GraphTraverseInfo>) -> HashMap<NodeId,NodeId> {
    let mut alias_sets = partition_into_alias_sets(g);
    let mut result : HashMap<NodeId,NodeId> = HashMap::new();
    for ids in alias_sets.values_mut() {
        ids.sort();
        check_sibling_conflict(ids,flow_info,&mut result);
    }
    for (id,n) in g.nodes.iter_enumerated() {
        let mut children = downward.get(&id).unwrap().clone();
        children = children.into_iter().filter(|x| copy_or_offset(g.nodes.get(*x).unwrap())).collect::<Vec<NodeId>>();
        if !children.is_empty() {
            check_sibling_conflict(&mut children,flow_info,&mut result);
        }
    }
    for id in g.nodes.indices() {
        let mut children = downward.get(&id).unwrap().clone();
        match children.iter().find(|cidx| result.get(cidx).is_some()){
            None => (),
            Some(failchild) => {result.insert(id,*result.get(failchild).unwrap()); ()},
        }
    }
    for (id,n) in g.nodes.iter_enumerated() {
        if let Some(par) = n.source {
            if let Some(f) = result.get(&par){
                let failidx = f.clone();
                if copy_or_offset(n){
                    result.insert(id,failidx);
                }
            }
        }
    }
    result
}

impl Display for NodeInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        let s = [
            ("mut", self.flows_to_mutation),
            ("load", self.flows_to_load),
            ("+offset", self.flows_to_neg_offset),
            ("-offset", self.flows_to_neg_offset),
            ("non unique by", self.non_unique),
        ].into_iter()
        .filter_map(|(name,node)| Some((name,node?)))
        .format_with(", ", |(name, node), f| f(&format_args!("{name} {node}")));
        write!(f,"{}", s)
    }
}

//TODO: check whether this should fit name
fn copy_or_offset(n: &Node) -> bool {
    !matches!(n.kind, NodeKind::Field(_))
}

fn node_does_mutation(n: &Node) -> bool {
    matches!(n.kind, NodeKind::StoreAddr | NodeKind::StoreValue)
}

fn node_does_load(n: &Node) -> bool {
    matches!(n.kind, NodeKind::LoadAddr | NodeKind::LoadValue)
}

fn node_does_pos_offset(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Offset(x) if x > 0)
}

fn node_does_neg_offset(n: &Node) -> bool {
    matches!(n.kind, NodeKind::Offset(x) if x < 0)
}

/// Takes a list of graphs, creates a NodeInfo object for each node in each graph, filling it with
/// the correct flow information.
pub fn augment_with_info(pdg: &mut Graphs) {
    let gs: &mut IndexVec<GraphId, Graph> = &mut pdg.graphs;
    for g in gs {
        let flow_info = create_flow_info(g);
        let mut conflicting = determine_non_conflicting(&g,&collect_children(&g),&flow_info);
        for (idx,node) in g.nodes.iter_enumerated_mut(){
            let node_flow = flow_info.get(&idx).unwrap();
            node.node_info = Some(NodeInfo {
                flows_to_mutation: node_flow.flows_to_store,
                flows_to_load: node_flow.flows_to_load,
                flows_to_pos_offset: node_flow.flows_to_pos_offset,
                flows_to_neg_offset: node_flow.flows_to_neg_offset,
                non_unique: conflicting.remove(&idx)
            })
        }
    }
}


#[cfg(test)]
mod test {
    use c2rust_analysis_rt::mir_loc::Func;
    use rustc_middle::mir::Local;
    use super::*;

    fn mk_node(g: &mut Graph, kind: NodeKind, source: Option<NodeId>) -> NodeId {
        g.nodes.push(Node {
            function: Func {
                def_path_hash: (1, 2).into(),
                name: "fake_function".into(),
            },
            block: 0_u32.into(),
            statement_idx: 0,
            dest: None,
            kind,
            source,
            node_info: None,
            debug_info: "".into(),
        })
    }

    fn mk_addr_of_local(g: &mut Graph, local: impl Into<Local>) -> NodeId {
        mk_node(g, NodeKind::AddrOfLocal(local.into()), None)
    }

    fn mk_copy(g: &mut Graph, source: NodeId) -> NodeId {
        mk_node(g, NodeKind::Copy, Some(source))
    }

    fn mk_load_addr(g: &mut Graph, source: NodeId) -> NodeId {
        mk_node(g, NodeKind::LoadAddr, Some(source))
    }

    fn mk_store_addr(g: &mut Graph, source: NodeId) -> NodeId {
        mk_node(g, NodeKind::StoreAddr, Some(source))
    }

    fn build_pdg(g: Graph) -> Graphs {
        let mut pdg = Graphs::default();
        pdg.graphs.push(g);
        augment_with_info(&mut pdg);
        pdg
    }

    fn info(pdg: &Graphs, id: NodeId) -> &NodeInfo {
        pdg.graphs[0_u32.into()].nodes[id].node_info.as_ref().unwrap()
    }

    #[test]
    fn unique_interleave() {
        let mut g = Graph::default();
        // let mut a = 0;   // A
        // let b = &mut a;  // B1
        // *b = 0;          // B2
        // let c = &mut a;  // C1
        // *c = 0;          // C2
        // *b = 0;          // B3
        // *c = 0;          // C3
        //
        // A
        // +----.
        // B1   |
        // +-B2 |
        // |    C1
        // |    +-C2
        // B3   |
        //      C3
        let a = mk_addr_of_local(&mut g, 0_u32);
        let b1 = mk_copy(&mut g, a);
        let b2 = mk_store_addr(&mut g, b1);
        let c1 = mk_copy(&mut g, a);
        let c2 = mk_store_addr(&mut g, c1);
        let b3 = mk_store_addr(&mut g, b1);
        let c3 = mk_store_addr(&mut g, c1);

        let pdg = build_pdg(g);
        assert!(info(&pdg, a).non_unique.is_some());
        assert!(info(&pdg, b1).non_unique.is_some());
        assert!(info(&pdg, b2).non_unique.is_some());

        assert!(info(&pdg, b3).non_unique.is_some());
        assert!(info(&pdg, c1).non_unique.is_some());
        assert!(info(&pdg, c2).non_unique.is_some());
        assert!(info(&pdg, c3).non_unique.is_some());
    }

    #[test]
    fn unique_interleave_onesided() {
        let mut g = Graph::default();
        // let mut a = 0;   // A
        // let b = &mut a;  // B1
        // *b = 0;          // B2
        // let c = &mut a;  // C1
        // *c = 0;          // C2
        // *b = 0;          // B3
        //
        // A
        // +----.
        // B1   |
        // +-B2 |
        // |    C1
        // |    +-C2
        // B3
        let a = mk_addr_of_local(&mut g, 0_u32);
        let b1 = mk_copy(&mut g, a);
        let b2 = mk_store_addr(&mut g, b1);
        let c1 = mk_copy(&mut g, a);
        let c2 = mk_store_addr(&mut g, c1);
        let b3 = mk_store_addr(&mut g, b1);

        let pdg = build_pdg(g);
        assert!(info(&pdg, a).non_unique.is_some());
        assert!(info(&pdg, b1).non_unique.is_some());
        assert!(info(&pdg, b2).non_unique.is_some());
        assert!(info(&pdg, b3).non_unique.is_some());
        assert!(info(&pdg, c1).non_unique.is_some());
        assert!(info(&pdg, c2).non_unique.is_some());
    }

    #[test]
    fn unique_sub_borrow() {
        let mut g = Graph::default();
        // let mut a = 0;   // A
        // let b = &mut a;  // B1
        // *b = 0;          // B2
        // let c = &mut *b; // C1
        // *c = 0;          // C2
        // *b = 0;          // B3
        //
        // A
        // |
        // B1
        // +-B2
        // +----C1
        // |    +-C2
        // B3
        let a = mk_addr_of_local(&mut g, 0_u32);
        let b1 = mk_copy(&mut g, a);
        let b2 = mk_store_addr(&mut g, b1);
        let c1 = mk_copy(&mut g, b1);
        let c2 = mk_store_addr(&mut g, c1);
        let b3 = mk_store_addr(&mut g, b1);

        let pdg = build_pdg(g);
        assert!(info(&pdg, a).non_unique.is_none());
        assert!(info(&pdg, b1).non_unique.is_none());
        assert!(info(&pdg, b2).non_unique.is_none());
        assert!(info(&pdg, b3).non_unique.is_none());
        assert!(info(&pdg, c1).non_unique.is_none());
        assert!(info(&pdg, c2).non_unique.is_none());
    }

    #[test]
    fn unique_sub_borrow_bad() {
        let mut g = Graph::default();
        // let mut a = 0;   // A
        // let b = &mut a;  // B1
        // *b = 0;          // B2
        // let c = &mut *b; // C1
        // *c = 0;          // C2
        // *b = 0;          // B3
        // *c = 0;          // C3
        //
        // A
        // |
        // B1
        // +-B2
        // +----C1
        // |    +-C2
        // B3   |
        //      C3
        let a = mk_addr_of_local(&mut g, 0_u32);
        let b1 = mk_copy(&mut g, a);
        let b2 = mk_store_addr(&mut g, b1);
        let c1 = mk_copy(&mut g, b1);
        let c2 = mk_store_addr(&mut g, c1);
        let b3 = mk_store_addr(&mut g, b1);
        let c3 = mk_store_addr(&mut g, c1);

        let pdg = build_pdg(g);
        assert!(info(&pdg, a).non_unique.is_none());
        assert!(info(&pdg, b1).non_unique.is_some());
        assert!(info(&pdg, b2).non_unique.is_some());
        assert!(info(&pdg, b3).non_unique.is_some());
        assert!(info(&pdg, c1).non_unique.is_some());
        assert!(info(&pdg, c2).non_unique.is_some());
        assert!(info(&pdg, c3).non_unique.is_some());
    }
}
