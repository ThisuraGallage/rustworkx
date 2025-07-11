// Licensed under the Apache License, Version 2.0 (the "License"); you may
// not use this file except in compliance with the License. You may obtain
// a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS, WITHOUT
// WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied. See the
// License for the specific language governing permissions and limitations
// under the License.

#![allow(clippy::borrow_as_ptr, clippy::redundant_closure)]

use std::cmp;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::prelude::*;
use std::io::{BufReader, BufWriter};
use std::str;

use hashbrown::{HashMap, HashSet};
use rustworkx_core::dictmap::*;
use rustworkx_core::graph_ext::*;

use pyo3::exceptions::PyIndexError;
use pyo3::gc::PyVisit;
use pyo3::prelude::*;
use pyo3::types::{IntoPyDict, PyBool, PyDict, PyGenericAlias, PyList, PyString, PyTuple, PyType};
use pyo3::IntoPyObjectExt;
use pyo3::PyTraverseError;
use pyo3::Python;

use ndarray::prelude::*;
use num_traits::Zero;
use numpy::Complex64;
use numpy::PyReadonlyArray2;

use crate::iterators::NodeMap;

use super::dot_utils::build_dot;
use super::iterators::{EdgeIndexMap, EdgeIndices, EdgeList, NodeIndices, WeightedEdgeList};
use super::{
    find_node_by_weight, weight_callable, IsNan, NoEdgeBetweenNodes, NodesRemoved, StablePyGraph,
};

use crate::RxPyResult;
use petgraph::algo;
use petgraph::graph::{EdgeIndex, NodeIndex};
use petgraph::prelude::*;
use petgraph::visit::{
    EdgeIndexable, GraphBase, IntoEdgeReferences, IntoNodeReferences, NodeCount, NodeFiltered,
    NodeIndexable,
};

/// A class for creating undirected graphs
///
/// The PyGraph class is used to create an undirected graph. It can be a
/// multigraph (have multiple edges between nodes). Each node and edge
/// (although rarely used for edges) is indexed by an integer id. These ids
/// are stable for the lifetime of the graph object and on node or edge
/// deletions you can have holes in the list of indices for the graph.
/// Node indices will be reused on additions after removal. For example:
///
/// .. jupyter-execute::
///
///        import rustworkx as rx
///
///        graph = rx.PyGraph()
///        graph.add_nodes_from(list(range(5)))
///        graph.add_nodes_from(list(range(2)))
///        graph.remove_node(2)
///        print("After deletion:", graph.node_indices())
///        res_manual = graph.add_node(None)
///        print("After adding a new node:", graph.node_indices())
///
/// Additionally, each node and edge contains an arbitrary Python object as a
/// weight/data payload. You can use the index for access to the data payload
/// as in the following example:
///
/// .. jupyter-execute::
///
///     import rustworkx as rx
///
///     graph = rx.PyGraph()
///     data_payload = "An arbitrary Python object"
///     node_index = graph.add_node(data_payload)
///     print("Node Index: %s" % node_index)
///     print(graph[node_index])
///
/// The PyGraph implements the Python mapping protocol for nodes so in
/// addition to access you can also update the data payload with:
///
/// .. jupyter-execute::
///
///     import rustworkx as rx
///
///     graph = rx.PyGraph()
///     data_payload = "An arbitrary Python object"
///     node_index = graph.add_node(data_payload)
///     graph[node_index] = "New Payload"
///     print("Node Index: %s" % node_index)
///     print(graph[node_index])
///
/// By default a ``PyGraph`` is a multigraph (meaning there can be parallel
/// edges between nodes) however this can be disabled by setting the
/// ``multigraph`` kwarg to ``False`` when calling the ``PyGraph``
/// constructor. For example::
///
///     import rustworkx as rx
///     graph = rx.PyGraph(multigraph=False)
///
/// This can only be set at ``PyGraph`` initialization and not adjusted after
/// creation. When :attr:`~rustworkx.PyGraph.multigraph` is set to ``False``
/// if a method call is made that would add a parallel edge it will instead
/// update the existing edge's weight/data payload.
///
/// Each ``PyGraph`` object has an :attr:`~.PyGraph.attrs` attribute which is
/// used to contain additional attributes/metadata of the graph instance. By
/// default this is set to ``None`` but can optionally be specified by using the
/// ``attrs`` keyword argument when constructing a new graph::
///
///     graph = rustworkx.PyGraph(attrs=dict(source_path='/tmp/graph.csv'))
///
/// This attribute can be set to any Python object. Additionally, you can access
/// and modify this attribute after creating an object. For example::
///
///     source_path = graph.attrs
///     graph.attrs = {'new_path': '/tmp/new.csv', 'old_path': source_path}
///
/// The maximum number of nodes and edges allowed on a ``PyGraph`` object is
/// :math:`2^{32} - 1` (4,294,967,294) each. Attempting to add more nodes or
/// edges than this will result in an exception being raised.
///
/// :param bool multigraph: When this is set to ``False`` the created PyGraph
///     object will not be a multigraph. When ``False`` if a method call is
///     made that would add parallel edges the weight/weight from that
///     method call will be used to update the existing edge in place.
/// :param Any attrs: An optional attributes payload to assign to the
///     :attr:`~.PyGraph.attrs` attribute. This can be any Python object. If
///     it is not specified :attr:`~.PyGraph.attrs` will be set to ``None``.
/// :param int node_count_hint: An optional hint that will allocate enough capacity to store this
///     many nodes before needing to grow. This does not prepopulate any nodes with data, it is
///     only a potential performance optimization if the complete size of the graph is known in
///     advance.
/// :param int edge_count_hint: An optional hint that will allocate enough capacity to store this
///     many edges before needing to grow.  This does not prepopulate any edges with data, it is
///     only a potential performance optimization if the complete size of the graph is known in
///     advance.
#[pyclass(mapping, module = "rustworkx", subclass)]
#[derive(Clone)]
pub struct PyGraph {
    pub graph: StablePyGraph<Undirected>,
    pub node_removed: bool,
    pub multigraph: bool,
    #[pyo3(get, set)]
    pub attrs: PyObject,
}

impl GraphBase for PyGraph {
    type NodeId = NodeIndex;
    type EdgeId = EdgeIndex;
}

impl NodesRemoved for &PyGraph {
    fn nodes_removed(&self) -> bool {
        self.node_removed
    }
}

impl NodeCount for PyGraph {
    fn node_count(&self) -> usize {
        self.graph.node_count()
    }
}

impl PyGraph {
    fn _add_edge(&mut self, u: NodeIndex, v: NodeIndex, edge: PyObject) -> usize {
        if !self.multigraph {
            let exists = self.graph.find_edge(u, v);
            if let Some(index) = exists {
                let edge_weight = self.graph.edge_weight_mut(index).unwrap();
                *edge_weight = edge;
                return index.index();
            }
        }
        let edge = self.graph.add_edge(u, v, edge);
        edge.index()
    }
}

#[pymethods]
impl PyGraph {
    #[new]
    #[pyo3(signature=(multigraph=true, attrs=None, *, node_count_hint=None, edge_count_hint=None))]
    fn new(
        py: Python,
        multigraph: bool,
        attrs: Option<PyObject>,
        node_count_hint: Option<usize>,
        edge_count_hint: Option<usize>,
    ) -> Self {
        PyGraph {
            graph: StablePyGraph::<Undirected>::with_capacity(
                node_count_hint.unwrap_or_default(),
                edge_count_hint.unwrap_or_default(),
            ),
            node_removed: false,
            multigraph,
            attrs: attrs.unwrap_or_else(|| py.None()),
        }
    }

    fn __getnewargs_ex__<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<(Bound<'py, PyTuple>, Bound<'py, PyDict>)> {
        Ok((
            (self.multigraph, self.attrs.clone_ref(py)).into_pyobject(py)?,
            [
                ("node_count_hint", self.graph.node_bound()),
                ("edge_count_hint", self.graph.edge_bound()),
            ]
            .into_py_dict(py)?,
        ))
    }

    fn __getstate__(&self, py: Python) -> PyResult<PyObject> {
        let mut nodes: Vec<PyObject> = Vec::with_capacity(self.graph.node_bound());
        let mut edges: Vec<PyObject> = Vec::with_capacity(self.graph.edge_bound());

        // save nodes to a list along with its index
        for node_idx in self.graph.node_indices() {
            let node_data = self.graph.node_weight(node_idx).unwrap();
            nodes.push((node_idx.index(), node_data).into_py_any(py)?);
        }

        // edges are saved with none (deleted edges) instead of their index to save space
        for i in 0..self.graph.edge_bound() {
            let idx = EdgeIndex::new(i);
            let edge = match self.graph.edge_weight(idx) {
                Some(edge_w) => {
                    let endpoints = self.graph.edge_endpoints(idx).unwrap();
                    (endpoints.0.index(), endpoints.1.index(), edge_w).into_py_any(py)?
                }
                None => py.None(),
            };
            edges.push(edge);
        }

        let out_dict = PyDict::new(py);
        let nodes_lst: PyObject = PyList::new(py, nodes)?.into_any().unbind();
        let edges_lst: PyObject = PyList::new(py, edges)?.into_any().unbind();
        out_dict.set_item("nodes", nodes_lst)?;
        out_dict.set_item("edges", edges_lst)?;
        out_dict.set_item("nodes_removed", self.node_removed)?;
        Ok(out_dict.into())
    }

    fn __setstate__(&mut self, py: Python, state: PyObject) -> PyResult<()> {
        let dict_state = state.downcast_bound::<PyDict>(py)?;
        let binding = dict_state.get_item("nodes")?.unwrap();
        let nodes_lst = binding.downcast::<PyList>()?;
        let binding = dict_state.get_item("edges")?.unwrap();
        let edges_lst = binding.downcast::<PyList>()?;

        self.node_removed = dict_state
            .get_item("nodes_removed")?
            .unwrap()
            .downcast::<PyBool>()?
            .extract()?;
        // graph is empty, stop early
        if nodes_lst.is_empty() {
            return Ok(());
        }

        if !self.node_removed {
            for item in nodes_lst.iter() {
                let node_w = item
                    .downcast::<PyTuple>()
                    .unwrap()
                    .get_item(1)
                    .unwrap()
                    .extract()
                    .unwrap();
                self.graph.add_node(node_w);
            }
        } else if nodes_lst.len() == 1 {
            // graph has only one node, handle logic here to save one if in the loop later
            let binding = nodes_lst.get_item(0).unwrap();
            let item = binding.downcast::<PyTuple>().unwrap();
            let node_idx: usize = item.get_item(0).unwrap().extract().unwrap();
            let node_w = item.get_item(1).unwrap().extract().unwrap();

            for _i in 0..node_idx {
                self.graph.add_node(py.None());
            }
            self.graph.add_node(node_w);
            for i in 0..node_idx {
                self.graph.remove_node(NodeIndex::new(i));
            }
        } else {
            let binding = nodes_lst.get_item(nodes_lst.len() - 1).unwrap();
            let last_item = binding.downcast::<PyTuple>().unwrap();

            // list of temporary nodes that will be removed later to re-create holes
            let node_bound_1: usize = last_item.get_item(0).unwrap().extract().unwrap();
            let mut tmp_nodes: Vec<NodeIndex> =
                Vec::with_capacity(node_bound_1 + 1 - nodes_lst.len());

            for item in nodes_lst {
                let item = item.downcast::<PyTuple>().unwrap();
                let next_index: usize = item.get_item(0).unwrap().extract().unwrap();
                let weight: PyObject = item.get_item(1).unwrap().extract().unwrap();
                while next_index > self.graph.node_bound() {
                    // node does not exist
                    let tmp_node = self.graph.add_node(py.None());
                    tmp_nodes.push(tmp_node);
                }
                // add node to the graph, and update the next available node index
                self.graph.add_node(weight);
            }
            // Remove any temporary nodes we added
            for tmp_node in tmp_nodes {
                self.graph.remove_node(tmp_node);
            }
        }

        // to ensure O(1) on edge deletion, use a temporary node to store missing edges
        let tmp_node = self.graph.add_node(py.None());

        for item in edges_lst {
            if item.is_none() {
                // add a temporary edge that will be deleted later to re-create the hole
                self.graph.add_edge(tmp_node, tmp_node, py.None());
            } else {
                let triple = item.downcast::<PyTuple>().unwrap();
                let edge_p: usize = triple.get_item(0).unwrap().extract().unwrap();
                let edge_c: usize = triple.get_item(1).unwrap().extract().unwrap();
                let edge_w = triple.get_item(2).unwrap().extract().unwrap();
                self.graph
                    .add_edge(NodeIndex::new(edge_p), NodeIndex::new(edge_c), edge_w);
            }
        }

        // remove the temporary node will remove all deleted edges in bulk,
        // the cost is equal to the number of edges
        self.graph.remove_node(tmp_node);

        Ok(())
    }

    /// Whether the graph is a multigraph (allows multiple edges between
    /// nodes) or not
    ///
    /// If set to ``False`` multiple edges between nodes are not allowed and
    /// calls that would add a parallel edge will instead update the existing
    /// edge
    #[getter]
    fn multigraph(&self) -> bool {
        self.multigraph
    }

    /// Detect if the graph has parallel edges or not
    ///
    /// :returns: ``True`` if the graph has parallel edges, ``False`` otherwise
    /// :rtype: bool
    #[pyo3(text_signature = "(self)")]
    fn has_parallel_edges(&self) -> bool {
        if !self.multigraph {
            return false;
        }
        self.graph.has_parallel_edges()
    }

    /// Clears all nodes and edges
    #[pyo3(text_signature = "(self)")]
    pub fn clear(&mut self) {
        self.graph.clear();
        self.node_removed = true;
    }

    /// Clears all edges, leaves nodes intact
    #[pyo3(text_signature = "(self)")]
    pub fn clear_edges(&mut self) {
        self.graph.clear_edges();
    }

    /// Return the number of nodes in the graph
    #[pyo3(text_signature = "(self)")]
    pub fn num_nodes(&self) -> usize {
        self.graph.node_count()
    }

    /// Return the number of edges in the graph
    #[pyo3(text_signature = "(self)")]
    pub fn num_edges(&self) -> usize {
        self.graph.edge_count()
    }

    /// Return a list of all edge data.
    ///
    /// :returns: A list of all the edge data objects in the graph
    /// :rtype: list[T]
    #[pyo3(text_signature = "(self)")]
    pub fn edges(&self) -> Vec<&PyObject> {
        self.graph
            .edge_indices()
            .map(|edge| self.graph.edge_weight(edge).unwrap())
            .collect()
    }

    /// Return a list of all edge indices.
    ///
    /// :returns: A list of all the edge indices in the graph
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self)")]
    pub fn edge_indices(&self) -> EdgeIndices {
        EdgeIndices {
            edges: self.graph.edge_indices().map(|edge| edge.index()).collect(),
        }
    }

    /// Return a list of indices of all edges between specified nodes
    ///
    /// :param int node_a: The index of the first node
    /// :param int node_b: The index of the second node
    ///
    /// :returns: A list of all the edge indices connecting the specified start and end node
    /// :rtype: EdgeIndices
    pub fn edge_indices_from_endpoints(&self, node_a: usize, node_b: usize) -> EdgeIndices {
        let node_a_index = NodeIndex::new(node_a);
        let node_b_index = NodeIndex::new(node_b);

        EdgeIndices {
            edges: self
                .graph
                .edges_directed(node_a_index, petgraph::Direction::Outgoing)
                .filter(|edge| edge.target() == node_b_index)
                .map(|edge| edge.id().index())
                .collect(),
        }
    }

    /// Return a list of all node data.
    ///
    /// :returns: A list of all the node data objects in the graph
    /// :rtype: list[S]
    #[pyo3(text_signature = "(self)")]
    pub fn nodes(&self) -> Vec<&PyObject> {
        self.graph
            .node_indices()
            .map(|node| self.graph.node_weight(node).unwrap())
            .collect()
    }

    /// Return a list of all node indices.
    ///
    /// :returns: A list of all the node indices in the graph
    /// :rtype: NodeIndices
    #[pyo3(text_signature = "(self)")]
    pub fn node_indices(&self) -> NodeIndices {
        NodeIndices {
            nodes: self.graph.node_indices().map(|node| node.index()).collect(),
        }
    }

    /// Return a list of all node indices.
    ///
    /// .. note::
    ///
    ///     This is identical to :meth:`.node_indices()`, which is the
    ///     preferred method to get the node indices in the graph. This
    ///     exists for backwards compatibility with earlier releases.
    ///
    /// :returns: A list of all the node indices in the graph
    /// :rtype: NodeIndices
    #[pyo3(text_signature = "(self)")]
    pub fn node_indexes(&self) -> NodeIndices {
        self.node_indices()
    }

    /// Check if the node exists in the graph.
    ///
    /// :param int node: The index of the node
    ///
    /// :returns: ``True`` if the node exists, ``False`` otherwise
    /// :rtype: bool
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn has_node(&self, node: usize) -> bool {
        let index = NodeIndex::new(node);
        self.graph.contains_node(index)
    }

    /// Check if there is any undirected edge between ``node_a`` and ``node_b``.
    ///
    /// :param int node_a: The index of the first node
    /// :param int node_b: The index of the second node
    ///
    /// :returns: ``True`` if the edge exists, ``False`` otherwise
    /// :rtype: bool
    #[pyo3(text_signature = "(self, node_a, node_b, /)")]
    pub fn has_edge(&self, node_a: usize, node_b: usize) -> bool {
        let index_a = NodeIndex::new(node_a);
        let index_b = NodeIndex::new(node_b);
        self.graph.find_edge(index_a, index_b).is_some()
    }

    ///  Return the edge data for the edge between 2 nodes.
    ///
    ///  Note if there are multiple edges between the nodes only one will be
    ///  returned. To get all edge data objects use
    ///  :meth:`~rustworkx.PyGraph.get_all_edge_data`
    ///
    /// :param int node_a: The index of the first node
    /// :param int node_b: The index of the second node
    ///
    /// :returns: The data object set for the edge
    /// :rtype: S
    /// :raises NoEdgeBetweenNodes: when there is no edge between the provided
    ///     nodes
    #[pyo3(text_signature = "(self, node_a, node_b, /)")]
    pub fn get_edge_data(&self, node_a: usize, node_b: usize) -> PyResult<&PyObject> {
        let index_a = NodeIndex::new(node_a);
        let index_b = NodeIndex::new(node_b);
        let edge_index = match self.graph.find_edge(index_a, index_b) {
            Some(edge_index) => edge_index,
            None => return Err(NoEdgeBetweenNodes::new_err("No edge found between nodes")),
        };

        let data = self.graph.edge_weight(edge_index).unwrap();
        Ok(data)
    }

    /// Return the list of edge indices incident to a provided node
    ///
    /// You can later retrieve the data payload of this edge with
    /// :meth:`~rustworkx.PyGraph.get_edge_data_by_index` or its
    /// endpoints with :meth:`~rustworkx.PyGraph.get_edge_endpoints_by_index`.
    ///
    /// :param int node: The node index to get incident edges from. If
    ///     this node index is not present in the graph this method will
    ///     return an empty list and not error.
    ///
    /// :returns: A list of the edge indices incident to a node in the graph
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn incident_edges(&self, node: usize) -> EdgeIndices {
        EdgeIndices {
            edges: self
                .graph
                .edges(NodeIndex::new(node))
                .map(|e| e.id().index())
                .collect(),
        }
    }

    /// Return the list of edge indices incident to a provided node.
    ///
    /// This method returns the indices of all edges connected to the provided
    /// ``node``. In undirected graphs, all edges connected to the node are
    /// returned as there is no distinction between incoming and outgoing edges.
    ///
    /// :param int node: The node index to get incident edges from. If
    ///     this node index is not present in the graph this method will
    ///     return an empty list and not error.
    ///
    /// :returns: A list of the edge indices incident to the node
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn in_edge_indices(&self, node: usize) -> EdgeIndices {
        EdgeIndices {
            edges: self
                .graph
                .edges(NodeIndex::new(node))
                .map(|e| e.id().index())
                .collect(),
        }
    }

    /// Return the list of edge indices incident to a provided node.
    ///
    /// This method returns the indices of all edges connected to the provided
    /// ``node``. In undirected graphs, all edges connected to the node are
    /// returned as there is no distinction between incoming and outgoing edges.
    ///
    /// :param int node: The node index to get incident edges from. If
    ///     this node index is not present in the graph this method will
    ///     return an empty list and not error.
    ///
    /// :returns: A list of the edge indices incident to the node
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn out_edge_indices(&self, node: usize) -> EdgeIndices {
        EdgeIndices {
            edges: self
                .graph
                .edges(NodeIndex::new(node))
                .map(|e| e.id().index())
                .collect(),
        }
    }

    /// Return the index map of edges incident to a provided node
    ///
    /// :param int node: The node index to get incident edges from. If
    ///     this node index is not present in the graph this method will
    ///     return an empty mapping and not error.
    ///
    /// :returns: A mapping of incident edge indices to the tuple
    ///     ``(source, target, data)``. The source endpoint node index in
    ///     this tuple will always be ``node`` to ensure you can easily
    ///     identify that the neighbor node is the second element in the
    ///     tuple for a given edge index.
    /// :rtype: EdgeIndexMap
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn incident_edge_index_map(&self, py: Python, node: usize) -> EdgeIndexMap {
        let node_index = NodeIndex::new(node);
        EdgeIndexMap {
            edge_map: self
                .graph
                .edges_directed(node_index, petgraph::Direction::Outgoing)
                .map(|edge| {
                    (
                        edge.id().index(),
                        (
                            edge.source().index(),
                            edge.target().index(),
                            edge.weight().clone_ref(py),
                        ),
                    )
                })
                .collect(),
        }
    }

    /// Get the endpoint indices and edge data for all edges of a node.
    ///
    /// This will return a list of tuples with the parent index, the node index
    /// and the edge data. This can be used to recreate add_edge() calls. As
    /// :class:`~rustworkx.PyGraph` is undirected this will return all edges
    /// with the second endpoint node index always being ``node``.
    ///
    /// :param int node: The index of the node to get the edges for
    ///
    /// :returns: A list of tuples of the form:
    ///     ``(parent_index, node_index, edge_data)```
    /// :rtype: WeightedEdgeList
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn in_edges(&self, py: Python, node: usize) -> WeightedEdgeList {
        let index = NodeIndex::new(node);
        let dir = petgraph::Direction::Incoming;
        let raw_edges = self.graph.edges_directed(index, dir);
        let out_list: Vec<(usize, usize, PyObject)> = raw_edges
            .map(|x| (x.source().index(), node, x.weight().clone_ref(py)))
            .collect();
        WeightedEdgeList { edges: out_list }
    }

    /// Get the endpoint indices and edge data for all edges of a node.
    ///
    /// This will return a list of tuples with the child index, the node index
    /// and the edge data. This can be used to recreate add_edge() calls. As
    /// :class:`~rustworkx.PyGraph` is undirected this will return all edges
    /// with the first endpoint node index always being ``node``.
    ///
    /// :param int node: The index of the node to get the edges for
    ///
    /// :returns out_edges: A list of tuples of the form:
    ///     ```(node_index, child_index, edge_data)```
    /// :rtype: WeightedEdgeList
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn out_edges(&self, py: Python, node: usize) -> WeightedEdgeList {
        let index = NodeIndex::new(node);
        let dir = petgraph::Direction::Outgoing;
        let raw_edges = self.graph.edges_directed(index, dir);
        let out_list: Vec<(usize, usize, PyObject)> = raw_edges
            .map(|x| (node, x.target().index(), x.weight().clone_ref(py)))
            .collect();
        WeightedEdgeList { edges: out_list }
    }

    /// Return the edge data for the edge by its given index
    ///
    /// :param int edge_index: The edge index to get the data for
    ///
    /// :returns: The data object for the edge
    /// :rtype: T
    /// :raises IndexError: when there is no edge present with the provided
    ///     index
    #[pyo3(text_signature = "(self, edge_index, /)")]
    pub fn get_edge_data_by_index(&self, edge_index: usize) -> PyResult<&PyObject> {
        let data = match self.graph.edge_weight(EdgeIndex::new(edge_index)) {
            Some(data) => data,
            None => {
                return Err(PyIndexError::new_err(format!(
                    "Provided edge index {edge_index} is not present in the graph"
                )));
            }
        };
        Ok(data)
    }

    /// Return the edge endpoints for the edge by its given index
    ///
    /// :param int edge_index: The edge index to get the endpoints for
    ///
    /// :returns: The endpoint tuple for the edge
    /// :rtype: tuple[int, int]
    /// :raises IndexError: when there is no edge present with the provided
    ///     index
    #[pyo3(text_signature = "(self, edge_index, /)")]
    pub fn get_edge_endpoints_by_index(&self, edge_index: usize) -> PyResult<(usize, usize)> {
        let endpoints = match self.graph.edge_endpoints(EdgeIndex::new(edge_index)) {
            Some(endpoints) => (endpoints.0.index(), endpoints.1.index()),
            None => {
                return Err(PyIndexError::new_err(format!(
                    "Provided edge index {edge_index} is not present in the graph"
                )));
            }
        };
        Ok(endpoints)
    }

    /// Update an edge's weight/payload in place
    ///
    /// If there are parallel edges in the graph only one edge will be updated.
    /// if you need to update a specific edge or need to ensure all parallel
    /// edges get updated you should use
    /// :meth:`~rustworkx.PyGraph.update_edge_by_index` instead.
    ///
    /// :param int source: The index of the first node
    /// :param int target: The index of the second node
    ///
    /// :raises NoEdgeBetweenNodes: When there is no edge between nodes
    #[pyo3(text_signature = "(self, source, target, edge, /)")]
    pub fn update_edge(&mut self, source: usize, target: usize, edge: PyObject) -> PyResult<()> {
        let index_a = NodeIndex::new(source);
        let index_b = NodeIndex::new(target);
        let edge_index = match self.graph.find_edge(index_a, index_b) {
            Some(edge_index) => edge_index,
            None => return Err(NoEdgeBetweenNodes::new_err("No edge found between nodes")),
        };
        let data = self.graph.edge_weight_mut(edge_index).unwrap();
        *data = edge;
        Ok(())
    }

    /// Update an edge's weight/data payload in place by the edge index
    ///
    /// :param int edge_index: The index of the edge
    /// :param T edge: The python object to attach to the edge
    ///
    /// :raises IndexError: when there is no edge present with the provided
    ///     index
    #[pyo3(text_signature = "(self, edge_index, edge, /)")]
    pub fn update_edge_by_index(&mut self, edge_index: usize, edge: PyObject) -> PyResult<()> {
        match self.graph.edge_weight_mut(EdgeIndex::new(edge_index)) {
            Some(data) => *data = edge,
            None => return Err(PyIndexError::new_err("No edge found for index")),
        };
        Ok(())
    }

    /// Return the node data for a given node index
    ///
    /// :param int node: The index of the node
    ///
    /// :returns: The data object set for that node
    /// :rtype: S
    /// :raises IndexError: when an invalid node index is provided
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn get_node_data(&self, node: usize) -> PyResult<&PyObject> {
        let index = NodeIndex::new(node);
        let node = match self.graph.node_weight(index) {
            Some(node) => node,
            None => return Err(PyIndexError::new_err("No node found for index")),
        };
        Ok(node)
    }

    /// Return the edge data for all the edges between 2 nodes.
    ///
    /// :param int node_a: The index of the first node
    /// :param int node_b: The index of the second node
    ///
    /// :returns: A list with all the data objects for the edges between nodes
    /// :rtype: list[T]
    /// :raises NoEdgeBetweenNodes: When there is no edge between nodes
    #[pyo3(text_signature = "(self, node_a, node_b, /)")]
    pub fn get_all_edge_data(&self, node_a: usize, node_b: usize) -> PyResult<Vec<&PyObject>> {
        let index_a = NodeIndex::new(node_a);
        let index_b = NodeIndex::new(node_b);
        let out: Vec<&PyObject> = self
            .graph
            .edges(index_a)
            .filter(|edge| edge.target() == index_b)
            .map(|edge| edge.weight())
            .collect();
        if out.is_empty() {
            Err(NoEdgeBetweenNodes::new_err("No edge found between nodes"))
        } else {
            Ok(out)
        }
    }

    /// Get edge list
    ///
    /// Returns a list of tuples of the form ``(source, target)`` where
    /// ``source`` and ``target`` are the node indices.
    ///
    /// :returns: An edge list without weights
    /// :rtype: EdgeList
    #[pyo3(text_signature = "(self)")]
    pub fn edge_list(&self) -> EdgeList {
        EdgeList {
            edges: self
                .graph
                .edge_references()
                .map(|edge| (edge.source().index(), edge.target().index()))
                .collect(),
        }
    }

    /// Get edge list with weights
    ///
    /// Returns a list of tuples of the form ``(source, target, weight)`` where
    /// ``source`` and ``target`` are the node indices and ``weight`` is the
    /// payload of the edge.
    ///
    /// :returns: An edge list with weights
    /// :rtype: WeightedEdgeList
    #[pyo3(text_signature = "(self)")]
    pub fn weighted_edge_list(&self, py: Python) -> WeightedEdgeList {
        WeightedEdgeList {
            edges: self
                .graph
                .edge_references()
                .map(|edge| {
                    (
                        edge.source().index(),
                        edge.target().index(),
                        edge.weight().clone_ref(py),
                    )
                })
                .collect(),
        }
    }

    /// Get an edge index map
    ///
    /// Returns a read only mapping from edge indices to the weighted edge
    /// tuple in the form: ``{0: (0, 1, "weight"), 1: (2, 3, 2.3)}``
    ///
    /// :returns: An edge index map
    /// :rtype: EdgeIndexMap
    #[pyo3(text_signature = "(self)")]
    pub fn edge_index_map(&self, py: Python) -> EdgeIndexMap {
        EdgeIndexMap {
            edge_map: self
                .graph
                .edge_references()
                .map(|edge| {
                    (
                        edge.id().index(),
                        (
                            edge.source().index(),
                            edge.target().index(),
                            edge.weight().clone_ref(py),
                        ),
                    )
                })
                .collect(),
        }
    }

    /// Remove a node from the graph.
    ///
    /// :param int node: The index of the node to remove. If the index is not
    ///     present in the graph it will be ignored and this function will
    ///     have no effect.
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn remove_node(&mut self, node: usize) -> PyResult<()> {
        let index = NodeIndex::new(node);
        self.graph.remove_node(index);
        self.node_removed = true;
        Ok(())
    }

    /// Add an edge between 2 nodes.
    ///
    /// If :attr:`~rustworkx.PyGraph.multigraph` is ``False`` and an edge already
    /// exists between ``node_a`` and ``node_b`` the weight/payload of that
    /// existing edge will be updated to be ``edge``.
    ///
    /// :param int node_a: The index of the parent node
    /// :param int node_b: The index of the child node
    /// :param T edge: The python object to attach to the edge
    ///
    /// :returns: The index of the newly created (or updated in the case
    ///     of an existing edge with ``multigraph=False``) edge.
    /// :rtype: int
    #[pyo3(text_signature = "(self, node_a, node_b, edge, /)")]
    pub fn add_edge(&mut self, node_a: usize, node_b: usize, edge: PyObject) -> PyResult<usize> {
        let p_index = NodeIndex::new(node_a);
        let c_index = NodeIndex::new(node_b);
        if !self.graph.contains_node(p_index) || !self.graph.contains_node(c_index) {
            return Err(PyIndexError::new_err(
                "One of the endpoints of the edge does not exist in graph",
            ));
        }
        Ok(self._add_edge(p_index, c_index, edge))
    }

    /// Add new edges to the graph.
    ///
    /// :param iterable[tuple[int, int, T]] obj_list: An iterable of tuples of the form
    ///     ``(node_a, node_b, T)`` to attach to the graph. ``node_a`` and
    ///     ``node_b`` are integer indices describing where an edge should be
    ///     added, and ``T`` is the python object for the edge data.
    ///
    /// If :attr:`~rustworkx.PyGraph.multigraph` is ``False`` and an edge already
    /// exists between ``node_a`` and ``node_b`` the weight/payload of that
    /// existing edge will be updated to be ``edge``. This will occur in order
    /// from ``obj_list`` so if there are multiple parallel edges in ``obj_list``
    /// the last entry will be used.
    ///
    /// :returns: A list of indices of the newly created edges
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, obj_list, /)")]
    pub fn add_edges_from(&mut self, obj_list: Bound<'_, PyAny>) -> PyResult<EdgeIndices> {
        let mut out_list = Vec::new();
        for py_obj in obj_list.try_iter()? {
            let obj = py_obj?.extract::<(usize, usize, PyObject)>()?;
            out_list.push(self.add_edge(obj.0, obj.1, obj.2)?);
        }
        Ok(EdgeIndices { edges: out_list })
    }

    /// Add new edges to the graph without python data.
    ///
    /// :param iterable[tuple[int, int]] obj_list: An iterable of tuples of the form
    ///     ``(parent, child)`` to attach to the graph. ``parent`` and
    ///     ``child`` are integer indices describing where an edge should be
    ///     added. Unlike :meth:`add_edges_from` there is no data payload and
    ///     when the edge is created None will be used.
    ///
    /// If :attr:`~rustworkx.PyGraph.multigraph` is ``False`` and an edge already
    /// exists between ``node_a`` and ``node_b`` the weight/payload of that
    /// existing edge will be updated to be ``None``.
    ///
    /// :returns: A list of indices of the newly created edges
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, obj_list, /)")]
    pub fn add_edges_from_no_data(
        &mut self,
        py: Python,
        obj_list: Bound<'_, PyAny>,
    ) -> PyResult<EdgeIndices> {
        let mut out_list: Vec<usize> = Vec::new();
        for py_obj in obj_list.try_iter()? {
            let obj = py_obj?.extract::<(usize, usize)>()?;
            out_list.push(self.add_edge(obj.0, obj.1, py.None())?);
        }
        Ok(EdgeIndices { edges: out_list })
    }

    /// Extend graph from an edge list
    ///
    /// This method differs from :meth:`add_edges_from_no_data` in that it will
    /// add nodes if a node index is not present in the edge list.
    ///
    /// If :attr:`~rustworkx.PyGraph.multigraph` is ``False`` and an edge already
    /// exists between ``node_a`` and ``node_b`` the weight/payload of that
    /// existing edge will be updated to be ``None``.
    ///
    /// :param iterable[tuple[int, int]] edge_list: An iterable of tuples
    ///     in the form ``(source, target)`` where ``source`` and ``target``
    ///     are integer node indices. If the node index
    ///     is not present in the graph, nodes will be added (with a node
    ///     weight of ``None``) to that index.
    #[pyo3(text_signature = "(self, edge_list, /)")]
    pub fn extend_from_edge_list(
        &mut self,
        py: Python,
        edge_list: Bound<'_, PyAny>,
    ) -> PyResult<()> {
        for py_obj in edge_list.try_iter()? {
            let (source, target) = py_obj?.extract::<(usize, usize)>()?;
            let max_index = cmp::max(source, target);
            while max_index >= self.node_count() {
                self.graph.add_node(py.None());
            }
            let source_index = NodeIndex::new(source);
            let target_index = NodeIndex::new(target);
            self._add_edge(source_index, target_index, py.None());
        }
        Ok(())
    }

    /// Extend graph from a weighted edge list
    ///
    /// This method differs from :meth:`add_edges_from` in that it will
    /// add nodes if a node index is not present in the edge list.
    ///
    /// If :attr:`~rustworkx.PyGraph.multigraph` is ``False`` and an edge already
    /// exists between ``node_a`` and ``node_b`` the weight/payload of that
    /// existing edge will be updated to be ``edge``. This will occur in order
    /// from ``obj_list`` so if there are multiple parallel edges in ``obj_list``
    /// the last entry will be used.
    ///
    /// :param iterable[tuple[int, int, T]] edge_list: An iterable of tuples in the form
    ///     ``(source, target, weight)`` where source and target are integer
    ///     node indices. If the node index is not present in the graph,
    ///     nodes will be added (with a node weight of ``None``) to that index.
    #[pyo3(text_signature = "(self, edge_list, /)")]
    pub fn extend_from_weighted_edge_list(
        &mut self,
        py: Python,
        edge_list: Bound<'_, PyAny>,
    ) -> PyResult<()> {
        for py_obj in edge_list.try_iter()? {
            let (source, target, weight) = py_obj?.extract::<(usize, usize, PyObject)>()?;
            let max_index = cmp::max(source, target);
            while max_index >= self.node_count() {
                self.graph.add_node(py.None());
            }
            let source_index = NodeIndex::new(source);
            let target_index = NodeIndex::new(target);
            self._add_edge(source_index, target_index, weight);
        }
        Ok(())
    }

    /// Remove an edge between 2 nodes.
    ///
    /// Note if there are multiple edges between the specified nodes only one
    /// will be removed.
    ///
    /// :param int parent: The index of the parent node
    /// :param int child: The index of the child node
    ///
    /// :raises NoEdgeBetweenNodes: If there is no edge between the nodes
    ///     specified
    #[pyo3(text_signature = "(self, node_a, node_b, /)")]
    pub fn remove_edge(&mut self, node_a: usize, node_b: usize) -> PyResult<()> {
        let p_index = NodeIndex::new(node_a);
        let c_index = NodeIndex::new(node_b);
        let edge_index = match self.graph.find_edge(p_index, c_index) {
            Some(edge_index) => edge_index,
            None => return Err(NoEdgeBetweenNodes::new_err("No edge found between nodes")),
        };
        self.graph.remove_edge(edge_index);
        Ok(())
    }

    /// Remove an edge identified by the provided index
    ///
    /// :param int edge: The index of the edge to remove
    #[pyo3(text_signature = "(self, edge, /)")]
    pub fn remove_edge_from_index(&mut self, edge: usize) -> PyResult<()> {
        let edge_index = EdgeIndex::new(edge);
        self.graph.remove_edge(edge_index);
        Ok(())
    }

    /// Remove edges from the graph.
    ///
    /// Note if there are multiple edges between the specified nodes only one
    /// will be removed.
    ///
    /// :param iterable[tuple[int, int]] index_list: An iterable of node index pairs
    ///     to remove from the graph
    ///
    /// :raises NoEdgeBetweenNodes: If there are no edges between a specified
    ///     pair of nodes.
    #[pyo3(text_signature = "(self, index_list, /)")]
    pub fn remove_edges_from(&mut self, index_list: Bound<'_, PyAny>) -> PyResult<()> {
        for py_obj in index_list.try_iter()? {
            let (x, y) = py_obj?.extract::<(usize, usize)>()?;
            let (p_index, c_index) = (NodeIndex::new(x), NodeIndex::new(y));
            let edge_index = match self.graph.find_edge(p_index, c_index) {
                Some(edge_index) => edge_index,
                None => return Err(NoEdgeBetweenNodes::new_err("No edge found between nodes")),
            };
            self.graph.remove_edge(edge_index);
        }
        Ok(())
    }

    /// Add a new node to the graph.
    ///
    /// :param S obj: The python object to attach to the node
    ///
    /// :returns: The index of the newly created node
    /// :rtype: int
    #[pyo3(text_signature = "(self, obj, /)")]
    pub fn add_node(&mut self, obj: PyObject) -> PyResult<usize> {
        let index = self.graph.add_node(obj);
        Ok(index.index())
    }

    /// Add new nodes to the graph.
    ///
    /// :param iterable[S] obj_list: An iterable of python object to attach to the graph
    ///
    /// :returns indices: A list of indices of the newly created nodes
    /// :rtype: NodeIndices
    #[pyo3(text_signature = "(self, obj_list, /)")]
    pub fn add_nodes_from(&mut self, obj_list: Bound<'_, PyAny>) -> PyResult<NodeIndices> {
        let mut out_list = Vec::new();
        for py_obj in obj_list.try_iter()? {
            let obj = py_obj?.extract::<PyObject>()?;
            out_list.push(self.graph.add_node(obj).index());
        }
        Ok(NodeIndices { nodes: out_list })
    }

    /// Remove nodes from the graph.
    ///
    /// If a node index in the list is not present in the graph it will be
    /// ignored.
    ///
    /// :param iterable[int] index_list: An iterable of node indices to remove from the
    ///     graph
    #[pyo3(text_signature = "(self, index_list, /)")]
    pub fn remove_nodes_from(&mut self, index_list: Bound<'_, PyAny>) -> PyResult<()> {
        for py_obj in index_list.try_iter()? {
            let node = py_obj?.extract::<usize>()?;
            self.remove_node(node)?;
        }
        Ok(())
    }

    /// Find node within this graph given a specific weight
    ///
    /// This algorithm has a worst case of O(n) since it searches the node
    /// indices in order. If there is more than one node in the graph with the
    /// same weight only the first match (by node index) will be returned.
    ///
    /// :param T obj: The weight to look for in the graph.
    ///
    /// :returns: the index of the first node in the graph that is equal to the
    ///     weight. If no match is found ``None`` will be returned.
    /// :rtype: int
    #[pyo3(text_signature = "(self, obj, /)")]
    pub fn find_node_by_weight(&self, py: Python, obj: PyObject) -> PyResult<Option<usize>> {
        find_node_by_weight(py, &self.graph, &obj).map(|node| node.map(|x| x.index()))
    }

    /// Get the index and data for the neighbors of a node.
    ///
    /// This will return a dictionary where the keys are the node indices of
    /// the adjacent nodes (inbound or outbound) and the value is the edge data
    /// objects between that adjacent node and the provided node. Note, that
    /// in the case of multigraphs only a single edge data object will be
    /// returned
    ///
    /// :param int node: The index of the node to get the neighbors of
    ///
    /// :returns neighbors: A dictionary where the keys are node indices and
    ///     the value is the edge data object for all nodes that share an
    ///     edge with the specified node.
    /// :rtype: dict[int, T]
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn adj(&mut self, node: usize) -> DictMap<usize, &PyObject> {
        let index = NodeIndex::new(node);
        self.graph
            .edges_directed(index, petgraph::Direction::Outgoing)
            .map(|edge| (edge.target().index(), edge.weight()))
            .collect()
    }

    /// Get the neighbors of a node.
    ///
    /// This with return a list of neighbor node indices
    ///
    /// :param int node: The index of the node to get the neighbors of
    ///
    /// :returns: A list of the neighbor node indices
    /// :rtype: NodeIndices
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn neighbors(&self, node: usize) -> NodeIndices {
        NodeIndices {
            nodes: self
                .graph
                .neighbors(NodeIndex::new(node))
                .map(|node| node.index())
                .collect::<HashSet<usize>>()
                .drain()
                .collect(),
        }
    }

    /// Get the degree for a node
    ///
    /// :param int node: The index of the node to find the inbound degree of
    ///
    /// :returns degree: The inbound degree of the specified node
    /// :rtype: int
    #[pyo3(text_signature = "(self, node, /)")]
    pub fn degree(&self, node: usize) -> usize {
        let index = NodeIndex::new(node);
        let neighbors = self.graph.edges(index);
        neighbors.fold(0, |count, edge| {
            if edge.source() == edge.target() {
                return count + 2;
            }
            count + 1
        })
    }

    /// Generate a new :class:`~rustworkx.PyDiGraph` object from this graph
    ///
    /// This will create a new :class:`~rustworkx.PyDiGraph` object from this
    /// graph. All edges in this graph will result in a bidirectional edge
    /// pair in the output graph.
    ///
    /// .. note::
    ///
    ///     The node indices in the output :class:`~rustworkx.PyDiGraph` may
    ///     differ if nodes have been removed.
    ///
    /// :returns: A new :class:`~rustworkx.PyDiGraph` object with a
    ///     bidirectional edge pair for each edge in this graph. Also all
    ///     node and edge weights/data payloads are copied by reference to
    ///     the output graph
    /// :rtype: PyDiGraph
    #[pyo3(text_signature = "(self)")]
    pub fn to_directed(&self, py: Python) -> crate::digraph::PyDiGraph {
        let node_count = self.node_count();
        let mut new_graph =
            StablePyGraph::<Directed>::with_capacity(node_count, 2 * self.graph.edge_count());
        let mut node_map: HashMap<NodeIndex, NodeIndex> = HashMap::with_capacity(node_count);
        for node_index in self.graph.node_indices() {
            let node = self.graph[node_index].clone_ref(py);
            let new_index = new_graph.add_node(node);
            node_map.insert(node_index, new_index);
        }
        for edge in self.graph.edge_references() {
            let &source = node_map.get(&edge.source()).unwrap();
            let &target = node_map.get(&edge.target()).unwrap();
            let weight = edge.weight();
            new_graph.add_edge(source, target, weight.clone_ref(py));
            new_graph.add_edge(target, source, weight.clone_ref(py));
        }
        crate::digraph::PyDiGraph {
            graph: new_graph,
            node_removed: false,
            cycle_state: algo::DfsSpace::default(),
            check_cycle: false,
            multigraph: self.multigraph,
            attrs: py.None(),
        }
    }

    /// Generate a dot file from the graph
    ///
    /// :param node_attr: A callable that will take in a node data object
    ///     and return a dictionary of attributes to be associated with the
    ///     node in the dot file. The key and value of this dictionary **must**
    ///     be a string. If they're not strings rustworkx will raise TypeError
    ///     (unfortunately without an error message because of current
    ///     limitations in the PyO3 type checking)
    /// :param edge_attr: A callable that will take in an edge data object
    ///     and return a dictionary of attributes to be associated with the
    ///     node in the dot file. The key and value of this dictionary **must**
    ///     be a string. If they're not strings rustworkx will raise TypeError
    ///     (unfortunately without an error message because of current
    ///     limitations in the PyO3 type checking)
    /// :param dict[str, str] graph_attr: An optional dictionary that specifies any graph
    ///     attributes for the output dot file. The key and value of this
    ///     dictionary **must** be a string. If they're not strings rustworkx
    ///     will raise TypeError (unfortunately without an error message
    ///     because of current limitations in the PyO3 type checking)
    /// :param str filename: An optional path to write the dot file to
    ///     if specified there is no return from the function
    ///
    /// :returns: A string with the dot file contents if filename is not
    ///     specified.
    /// :rtype: str
    ///
    /// Using this method enables you to leverage graphviz to visualize a
    /// :class:`rustworkx.PyGraph` object. For example:
    ///
    /// .. jupyter-execute::
    ///
    ///   import os
    ///   import tempfile
    ///
    ///   import pydot
    ///   from PIL import Image
    ///
    ///   import rustworkx as rx
    ///
    ///   graph = rx.undirected_gnp_random_graph(15, .25)
    ///   dot_str = graph.to_dot(
    ///       lambda node: dict(
    ///           color='black', fillcolor='lightblue', style='filled'))
    ///   dot = pydot.graph_from_dot_data(dot_str)[0]
    ///
    ///   with tempfile.TemporaryDirectory() as tmpdirname:
    ///       tmp_path = os.path.join(tmpdirname, 'dag.png')
    ///       dot.write_png(tmp_path)
    ///       image = Image.open(tmp_path)
    ///       os.remove(tmp_path)
    ///   image
    ///
    #[pyo3(
        text_signature = "(self, /, node_attr=None, edge_attr=None, graph_attr=None, filename=None)",
        signature = (node_attr=None, edge_attr=None, graph_attr=None, filename=None)
    )]
    pub fn to_dot<'py>(
        &self,
        py: Python<'py>,
        node_attr: Option<PyObject>,
        edge_attr: Option<PyObject>,
        graph_attr: Option<BTreeMap<String, String>>,
        filename: Option<String>,
    ) -> PyResult<Option<Bound<'py, PyString>>> {
        match filename {
            Some(filename) => {
                let mut file = File::create(filename)?;
                build_dot(py, &self.graph, &mut file, graph_attr, node_attr, edge_attr)?;
                Ok(None)
            }
            None => {
                let mut file = Vec::<u8>::new();
                build_dot(py, &self.graph, &mut file, graph_attr, node_attr, edge_attr)?;
                Ok(Some(PyString::new(py, str::from_utf8(&file)?)))
            }
        }
    }

    /// Read an edge list file and create a new PyGraph object from the
    /// contents
    ///
    /// The expected format of the edge list file is a line separated list
    /// of delimited node ids. If there are more than 3 elements on
    /// a line the 3rd on will be treated as a string weight for the edge
    ///
    /// :param str path: The path of the file to read from
    /// :param str comment: Optional character to use as a comment prefix
    ///     (by default there are no comment characters)
    /// :param str deliminator: Optional character to use as a deliminator
    ///     (by default any whitespace will be used)
    /// :param bool labels: If set to ``True`` the first two separated fields
    ///     will be treated as string labels uniquely identifying a node
    ///     instead of node indices
    ///
    /// For example:
    ///
    /// .. jupyter-execute::
    ///
    ///   import tempfile
    ///
    ///   import rustworkx as rx
    ///   from rustworkx.visualization import mpl_draw
    ///
    ///   with tempfile.NamedTemporaryFile('wt') as fd:
    ///       path = fd.name
    ///       fd.write('0 1\n')
    ///       fd.write('0 2\n')
    ///       fd.write('0 3\n')
    ///       fd.write('1 2\n')
    ///       fd.write('2 3\n')
    ///       fd.flush()
    ///       graph = rx.PyGraph.read_edge_list(path=path)
    ///   mpl_draw(graph)
    ///
    #[staticmethod]
    #[pyo3(signature=(path, comment=None, deliminator=None, labels=false),  text_signature = "(path, /, comment=None, deliminator=None, labels=False)")]
    pub fn read_edge_list(
        py: Python,
        path: &str,
        comment: Option<String>,
        deliminator: Option<String>,
        labels: bool,
    ) -> PyResult<PyGraph> {
        let file = File::open(path)?;
        let buf_reader = BufReader::new(file);
        let mut out_graph = StablePyGraph::<Undirected>::default();
        let mut label_map: HashMap<String, usize> = HashMap::new();
        for line_raw in buf_reader.lines() {
            let line = line_raw?;
            let skip = match &comment {
                Some(comm) => line.trim().starts_with(comm),
                None => line.trim().is_empty(),
            };
            if skip {
                continue;
            }
            let line_no_comments = match &comment {
                Some(comm) => line
                    .find(comm)
                    .map(|idx| &line[..idx])
                    .unwrap_or(&line)
                    .trim()
                    .to_string(),
                None => line,
            };
            let pieces: Vec<&str> = match &deliminator {
                Some(del) => line_no_comments.split(del).collect(),
                None => line_no_comments.split_whitespace().collect(),
            };
            let src: usize;
            let target: usize;
            if labels {
                let src_str = pieces[0];
                let target_str = pieces[1];
                src = match label_map.get(src_str) {
                    Some(index) => *index,
                    None => {
                        let index = out_graph.add_node(src_str.into_py_any(py)?).index();
                        label_map.insert(src_str.to_string(), index);
                        index
                    }
                };
                target = match label_map.get(target_str) {
                    Some(index) => *index,
                    None => {
                        let index = out_graph.add_node(target_str.into_py_any(py)?).index();
                        label_map.insert(target_str.to_string(), index);
                        index
                    }
                };
            } else {
                src = pieces[0].parse::<usize>()?;
                target = pieces[1].parse::<usize>()?;
                let max_index = cmp::max(src, target);
                // Add nodes to graph
                while max_index >= out_graph.node_count() {
                    out_graph.add_node(py.None());
                }
            }
            // Add edges tp graph
            let weight = if pieces.len() > 2 {
                let weight_str = match &deliminator {
                    Some(del) => pieces[2..].join(del),
                    None => pieces[2..].join(&' '.to_string()),
                };
                PyString::new(py, &weight_str).into_any().unbind()
            } else {
                py.None()
            };
            out_graph.add_edge(NodeIndex::new(src), NodeIndex::new(target), weight);
        }
        Ok(PyGraph {
            graph: out_graph,
            node_removed: false,
            multigraph: true,
            attrs: py.None(),
        })
    }

    /// Write an edge list file from the PyGraph object
    ///
    /// :param str path: The path to write the output file to
    /// :param str deliminator: The optional character to use as a deliminator
    ///     if not specified ``" "`` is used.
    /// :param callable weight_fn: An optional callback function that will be
    ///     passed an edge's data payload/weight object and is expected to
    ///     return a string (a ``TypeError`` will be raised if it doesn't
    ///     return a string). If specified the weight in the output file
    ///     for each edge will be set to the returned string.
    ///
    ///  For example:
    ///
    ///  .. jupyter-execute::
    ///
    ///     import os
    ///     import tempfile
    ///
    ///     import rustworkx as rx
    ///
    ///     graph = rx.generators.path_graph(5)
    ///     path = os.path.join(tempfile.gettempdir(), "edge_list")
    ///     graph.write_edge_list(path, deliminator=',')
    ///     # Print file contents
    ///     with open(path, 'rt') as edge_file:
    ///         print(edge_file.read())
    ///
    #[pyo3(text_signature = "(self, path, /, deliminator=None, weight_fn=None)", signature = (path, deliminator=None, weight_fn=None))]
    pub fn write_edge_list(
        &self,
        py: Python,
        path: &str,
        deliminator: Option<char>,
        weight_fn: Option<PyObject>,
    ) -> PyResult<()> {
        let file = File::create(path)?;
        let mut buf_writer = BufWriter::new(file);
        let delim = match deliminator {
            Some(delim) => delim.to_string(),
            None => " ".to_string(),
        };

        for edge in self.graph.edge_references() {
            buf_writer.write_all(
                format!(
                    "{}{}{}",
                    edge.source().index(),
                    delim,
                    edge.target().index()
                )
                .as_bytes(),
            )?;
            match weight_callable(py, &weight_fn, edge.weight(), None as Option<String>)? {
                Some(weight) => buf_writer.write_all(format!("{delim}{weight}\n").as_bytes()),
                None => buf_writer.write_all(b"\n"),
            }?;
        }
        buf_writer.flush()?;
        Ok(())
    }

    /// Create a new :class:`~rustworkx.PyGraph` object from an adjacency matrix
    /// with matrix elements of type ``float``
    ///
    /// This method can be used to construct a new :class:`~rustworkx.PyGraph`
    /// object from an input adjacency matrix. The node weights will be the
    /// index from the matrix. The edge weights will be a float value of the
    /// value from the matrix.
    ///
    /// This differs from the
    /// :meth:`~rustworkx.PyGraph.from_complex_adjacency_matrix` in that the
    /// type of the elements of input matrix must be a ``float`` (specifically
    /// a ``numpy.float64``) and the output graph edge weights will be ``float``
    /// too. While in :meth:`~rustworkx.PyGraph.from_complex_adjacency_matrix`
    /// the matrix elements are of type ``complex`` (specifically
    /// ``numpy.complex128``) and the edge weights in the output graph will be
    /// ``complex`` too.
    ///
    /// :param ndarray matrix: The input numpy array adjacency matrix to create
    ///     a new :class:`~rustworkx.PyGraph` object from. It must be a 2
    ///     dimensional array and be a ``float``/``np.float64`` data type.
    /// :param float null_value: An optional float that will treated as a null
    ///     value. If any element in the input matrix is this value it will be
    ///     treated as not an edge. By default this is ``0.0``.
    ///
    /// :returns: A new graph object generated from the adjacency matrix
    /// :rtype: PyGraph
    #[staticmethod]
    #[pyo3(signature=(matrix, null_value=0.0), text_signature = "(matrix, /, null_value=0.0)")]
    pub fn from_adjacency_matrix<'p>(
        py: Python<'p>,
        matrix: PyReadonlyArray2<'p, f64>,
        null_value: f64,
    ) -> PyResult<PyGraph> {
        _from_adjacency_matrix(py, matrix, null_value)
    }

    /// Create a new :class:`~rustworkx.PyGraph` object from an adjacency matrix
    /// with matrix elements of type ``complex``
    ///
    /// This method can be used to construct a new :class:`~rustworkx.PyGraph`
    /// object from an input adjacency matrix. The node weights will be the
    /// index from the matrix. The edge weights will be a complex value of the
    /// value from the matrix.
    ///
    /// This differs from the
    /// :meth:`~rustworkx.PyGraph.from_adjacency_matrix` in that the type of
    /// the elements of the input matrix in this method must be a ``complex``
    /// (specifically a ``numpy.complex128``) and the output graph edge weights
    /// will be ``complex`` too. While in
    /// :meth:`~rustworkx.PyGraph.from_adjacency_matrix` the matrix elements
    /// are of type ``float`` (specifically ``numpy.float64``) and the edge
    /// weights in the output graph will be ``float`` too.
    ///
    /// :param ndarray matrix: The input numpy array adjacency matrix to create
    ///     a new :class:`~rustworkx.PyGraph` object from. It must be a 2
    ///     dimensional array and be a ``complex``/``np.complex128`` data type.
    /// :param float null_value: An optional complex that will treated as a null
    ///     value. If any element in the input matrix is this value it will be
    ///     treated as not an edge. By default this is ``0.0+0.0j``
    ///
    /// :returns: A new graph object generated from the adjacency matrix
    /// :rtype: PyGraph
    ///
    #[staticmethod]
    #[pyo3(signature=(matrix, null_value=Complex64::zero()), text_signature = "(matrix, /, null_value=0.0+0.0j)")]
    pub fn from_complex_adjacency_matrix<'p>(
        py: Python<'p>,
        matrix: PyReadonlyArray2<'p, Complex64>,
        null_value: Complex64,
    ) -> PyResult<PyGraph> {
        _from_adjacency_matrix(py, matrix, null_value)
    }

    /// Add another PyGraph object into this PyGraph
    ///
    /// :param PyGraph other: The other PyGraph object to add onto this
    ///     graph.
    /// :param dict[int, tuple[int, tuple[int, T]]] node_map: A dictionary
    ///     mapping node indices from this
    ///     PyGraph object to node indices in the other PyGraph object.
    ///     The key is a node index in this graph and the value is a tuple
    ///     of the node index in the other graph to add an edge to and the
    ///     weight of that edge. For example::
    ///
    ///         {
    ///             1: (2, "weight"),
    ///             2: (4, "weight2")
    ///         }
    ///
    /// :param Callable node_map_func: An optional python callable that will take in a
    ///     single node weight/data object and return a new node weight/data
    ///     object that will be used when adding an node from other onto this
    ///     graph.
    /// :param Callable edge_map_func: An optional python callable that will take in a
    ///     single edge weight/data object and return a new edge weight/data
    ///     object that will be used when adding an edge from other onto this
    ///     graph.
    ///
    /// :returns: new_node_ids: A dictionary mapping node index from the other
    ///     PyGraph to the equivalent node index in this PyDAG after they've
    ///     been combined
    /// :rtype: dict[int, int]
    ///
    /// For example, start by building a graph:
    ///
    /// .. jupyter-execute::
    ///
    ///   import os
    ///   import tempfile
    ///
    ///   import pydot
    ///   from PIL import Image
    ///
    ///   import rustworkx as rx
    ///   from rustworkx.visualization import mpl_draw
    ///
    ///   # Build first graph and visualize:
    ///   graph = rx.PyGraph()
    ///   node_a, node_b, node_c = graph.add_nodes_from(['A', 'B', 'C'])
    ///   graph.add_edges_from([(node_a, node_b, 'A to B'),
    ///                         (node_b, node_c, 'B to C')])
    ///   mpl_draw(graph, with_labels=True, labels=str, edge_labels=str)
    ///
    /// Then build a second one:
    ///
    /// .. jupyter-execute::
    ///
    ///   # Build second graph and visualize:
    ///   other_graph = rx.PyGraph()
    ///   node_d, node_e = other_graph.add_nodes_from(['D', 'E'])
    ///   other_graph.add_edge(node_d, node_e, 'D to E')
    ///   mpl_draw(other_graph, with_labels=True, labels=str, edge_labels=str)
    ///
    /// Finally compose the ``other_graph`` onto ``graph``
    ///
    /// .. jupyter-execute::
    ///
    ///   node_map = {node_b: (node_d, 'B to D')}
    ///   graph.compose(other_graph, node_map)
    ///   mpl_draw(graph, with_labels=True, labels=str, edge_labels=str)
    ///
    #[pyo3(text_signature = "(self, other, node_map, /, node_map_func=None, edge_map_func=None)", signature = (other, node_map, node_map_func=None, edge_map_func=None))]
    pub fn compose(
        &mut self,
        py: Python,
        other: &PyGraph,
        node_map: HashMap<usize, (usize, PyObject)>,
        node_map_func: Option<PyObject>,
        edge_map_func: Option<PyObject>,
    ) -> PyResult<PyObject> {
        let mut new_node_map: DictMap<NodeIndex, NodeIndex> =
            DictMap::with_capacity(other.node_count());

        // TODO: Reimplement this without looping over the graphs
        // Loop over other nodes add to self graph
        for node in other.graph.node_indices() {
            let new_index = self.graph.add_node(weight_transform_callable(
                py,
                &node_map_func,
                &other.graph[node],
            )?);
            new_node_map.insert(node, new_index);
        }

        // loop over other edges and add to self graph
        for edge in other.graph.edge_references() {
            let new_p_index = new_node_map.get(&edge.source()).unwrap();
            let new_c_index = new_node_map.get(&edge.target()).unwrap();
            let weight = weight_transform_callable(py, &edge_map_func, edge.weight())?;
            self.graph.add_edge(*new_p_index, *new_c_index, weight);
        }
        // Add edges from map
        for (this_index, (index, weight)) in node_map.iter() {
            let new_index = new_node_map.get(&NodeIndex::new(*index)).unwrap();
            self.graph.add_edge(
                NodeIndex::new(*this_index),
                *new_index,
                weight.clone_ref(py),
            );
        }
        let out_dict = PyDict::new(py);
        for (orig_node, new_node) in new_node_map.iter() {
            out_dict.set_item(orig_node.index(), new_node.index())?;
        }
        Ok(out_dict.into())
    }

    /// Substitute a node with a PyGraph object
    ///
    /// :param int node: The index of the node to be replaced with the PyGraph object
    /// :param PyGraph other: The other graph to replace ``node`` with
    /// :param Callable edge_map_fn: A callable object that will take 3 position
    ///     parameters, ``(source, target, weight)`` to represent an edge either to
    ///     or from ``node`` in this graph. The expected return value from this
    ///     callable is the node index of the node in ``other`` that an edge should
    ///     be to/from. If None is returned, that edge will be skipped and not
    ///     be copied.
    /// :param Callable node_filter: An optional callable object that when used
    ///     will receive a node's payload object from ``other`` and return
    ///     ``True`` if that node is to be included in the graph or not.
    /// :param Callable edge_weight_map: An optional callable object that when
    ///     used will receive an edge's weight/data payload from ``other`` and
    ///     will return an object to use as the weight for a newly created edge
    ///     after the edge is mapped from ``other``. If not specified the weight
    ///     from the edge in ``other`` will be copied by reference and used.
    ///
    /// :returns: A mapping of node indices in ``other`` to the equivalent node
    ///     in this graph.
    /// :rtype: NodeMap
    ///
    /// .. note::
    ///
    ///    The return type is a :class:`rustworkx.NodeMap` which is an unordered
    ///    type. So it does not provide a deterministic ordering between objects
    ///    when iterated over (although the same object will have a consistent
    ///    order when iterated over multiple times).
    ///
    #[pyo3(
        text_signature = "(self, node, other, edge_map_fn, /, node_filter=None, edge_weight_map=None",
        signature = (node, other, edge_map_fn, node_filter=None, edge_weight_map=None)
    )]
    fn substitute_node_with_subgraph(
        &mut self,
        py: Python,
        node: usize,
        other: &PyGraph,
        edge_map_fn: PyObject,
        node_filter: Option<PyObject>,
        edge_weight_map: Option<PyObject>,
    ) -> PyResult<NodeMap> {
        let filter_fn = |obj: &PyObject, filter_fn: &Option<PyObject>| -> PyResult<bool> {
            match filter_fn {
                Some(filter) => {
                    let res = filter.call1(py, (obj,))?;
                    res.extract(py)
                }
                None => Ok(true),
            }
        };

        let weight_map_fn = |obj: &PyObject, weight_fn: &Option<PyObject>| -> PyResult<PyObject> {
            match weight_fn {
                Some(weight_fn) => weight_fn.call1(py, (obj,)),
                None => Ok(obj.clone_ref(py)),
            }
        };

        let map_fn = |source: usize, target: usize, weight: &PyObject| -> PyResult<Option<usize>> {
            let res = edge_map_fn.call1(py, (source, target, weight))?;
            res.extract(py)
        };

        let node_index = NodeIndex::new(node);
        if self.graph.node_weight(node_index).is_none() {
            return Err(PyIndexError::new_err(format!(
                "Specified node {node} is not in this graph"
            )));
        }

        // Copy all nodes from other to self
        let mut out_map: DictMap<usize, usize> = DictMap::with_capacity(other.node_count());
        for node in other.graph.node_indices() {
            let node_weight: Py<PyAny> = other.graph[node].clone_ref(py);
            if !filter_fn(&node_weight, &node_filter)? {
                continue;
            }
            let new_index: NodeIndex = self.graph.add_node(node_weight);
            out_map.insert(node.index(), new_index.index());
        }

        if out_map.is_empty() {
            self.graph.remove_node(node_index);
            return Ok(NodeMap {
                node_map: DictMap::new(),
            });
        }

        // Copy all edges
        for edge in other.graph.edge_references().filter(|edge| {
            out_map.contains_key(&edge.target().index())
                && out_map.contains_key(&edge.source().index())
        }) {
            self._add_edge(
                NodeIndex::new(out_map[&edge.source().index()]),
                NodeIndex::new(out_map[&edge.target().index()]),
                weight_map_fn(edge.weight(), &edge_weight_map)?,
            );
        }
        // Incoming and outgoing edges.
        let in_edges: Vec<(NodeIndex, NodeIndex, PyObject)> = self
            .graph
            .edge_references()
            .filter(|edge| edge.target() == node_index)
            .map(|edge| (edge.source(), edge.target(), edge.weight().clone_ref(py)))
            .collect();
        // Keep track of what's present on incoming edges
        let in_set: HashSet<(NodeIndex, NodeIndex)> =
            in_edges.iter().map(|edge| (edge.0, edge.1)).collect();
        // Retrieve outgoing edges. Make sure to not include any incoming edge.
        let out_edges: Vec<(NodeIndex, NodeIndex, PyObject)> = self
            .graph
            .edges(node_index)
            .filter(|edge| !in_set.contains(&(edge.target(), edge.source())))
            .map(|edge| (edge.source(), edge.target(), edge.weight().clone_ref(py)))
            .collect();
        for (source, target, weight) in in_edges {
            let old_index: Option<usize> = map_fn(source.index(), target.index(), &weight)?;
            let target_out: NodeIndex = match old_index {
                Some(old_index) => match out_map.get(&old_index) {
                    Some(new_index) => NodeIndex::new(*new_index),
                    None => {
                        return Err(PyIndexError::new_err(format!(
                            "No mapped index {old_index} found"
                        )))
                    }
                },
                None => continue,
            };
            self._add_edge(source, target_out, weight);
        }
        for (source, target, weight) in out_edges {
            let old_index: Option<usize> = map_fn(source.index(), target.index(), &weight)?;
            let source_out: NodeIndex = match old_index {
                Some(old_index) => match out_map.get(&old_index) {
                    Some(new_index) => NodeIndex::new(*new_index),
                    None => {
                        return Err(PyIndexError::new_err(format!(
                            "No mapped index {old_index} found"
                        )))
                    }
                },
                None => continue,
            };
            self._add_edge(source_out, target, weight);
        }
        // Remove original node
        self.graph.remove_node(node_index);
        Ok(NodeMap { node_map: out_map })
    }

    /// Substitute a set of nodes with a single new node.
    ///
    /// .. note::
    ///     This method does not preserve the ordering of endpoints in
    ///     edge tuple representations (e.g. the tuples returned from
    ///     :meth:`~rustworkx.PyGraph.edge_list`).
    ///
    /// :param list[int] nodes: A set of nodes to be removed and replaced
    ///     by the new node. Any nodes not in the graph are ignored.
    ///     If empty, this method behaves like :meth:`~PyGraph.add_node`
    ///     (but slower).
    /// :param S obj: The data/weight to associate with the new node.
    /// :param Callable weight_combo_fn: An optional python callable that, when
    ///     specified, is used to merge parallel edges introduced by the
    ///     contraction, which will occur if any two edges between ``nodes``
    ///     and the rest of the graph share an endpoint.
    ///     If this instance of :class:`~rustworkx.PyGraph` is a multigraph,
    ///     leave this unspecified to preserve parallel edges. If unspecified
    ///     when not a multigraph, parallel edges and their weights will be
    ///     combined by choosing one of the edge's weights arbitrarily based
    ///     on an internal iteration order, subject to change.
    /// :returns: The index of the newly created node.
    /// :rtype: int
    #[pyo3(text_signature = "(self, nodes, obj, /, weight_combo_fn=None)", signature = (nodes, obj, weight_combo_fn=None))]
    pub fn contract_nodes(
        &mut self,
        py: Python,
        nodes: Vec<usize>,
        obj: PyObject,
        weight_combo_fn: Option<PyObject>,
    ) -> RxPyResult<usize> {
        let nodes = nodes.into_iter().map(|i| NodeIndex::new(i));
        let res = match (weight_combo_fn, &self.multigraph) {
            (Some(user_callback), _) => {
                self.graph
                    .contract_nodes_simple(nodes, obj, |w1, w2| user_callback.call1(py, (w1, w2)))?
            }
            (None, false) => {
                // By default, just take first edge.
                self.graph.contract_nodes_simple(nodes, obj, move |w1, _| {
                    Ok::<_, PyErr>(w1.clone_ref(py))
                })?
            }
            (None, true) => self.graph.contract_nodes(nodes, obj),
        };
        Ok(res.index())
    }

    /// Return a new PyGraph object for a subgraph of this graph and a NodeMap
    /// object that maps the nodes of the subgraph to the nodes of the original graph.
    ///
    /// .. note::
    ///     This method is identical to :meth:`.subgraph()` but includes a
    ///     NodeMap object that maps the nodes of the subgraph to the nodes of
    ///     the original graph.
    ///
    /// :param list[int] nodes: A list of node indices to generate the subgraph
    ///     from. If a node index is included that is not present in the graph
    ///     it will silently be ignored.
    /// :param bool preserve_attrs: If set to the True the attributes of the PyGraph
    ///     will be copied by reference to be the attributes of the output
    ///     subgraph. By default this is set to False and the :attr:`~.PyGraph.attrs`
    ///     attribute will be ``None`` in the subgraph.
    ///
    /// :returns: A tuple containing a new PyGraph object representing a subgraph of this graph
    ///     and a NodeMap object that maps the nodes of the subgraph to the nodes of the original graph.
    ///     It is worth noting that node and edge weight/data payloads are
    ///     passed by reference so if you update (not replace) an object used
    ///     as the weight in graph or the subgraph it will also be updated in
    ///     the other.
    /// :rtype: tuple[PyGraph, NodeMap]
    ///
    #[pyo3(signature=(nodes, preserve_attrs=false), text_signature = "(self, nodes, /, preserve_attrs=False)")]
    pub fn subgraph_with_nodemap(
        &self,
        py: Python,
        nodes: Vec<usize>,
        preserve_attrs: bool,
    ) -> (PyGraph, NodeMap) {
        let node_set: HashSet<usize> = nodes.iter().cloned().collect();
        // mapping from original node index to new node index
        let mut node_map: HashMap<NodeIndex, NodeIndex> = HashMap::with_capacity(nodes.len());
        // mapping from new node index to original node index
        let mut node_dict: DictMap<usize, usize> = DictMap::with_capacity(nodes.len());
        let node_filter = |node: NodeIndex| -> bool { node_set.contains(&node.index()) };
        let mut out_graph = StablePyGraph::<Undirected>::default();
        let filtered = NodeFiltered(&self.graph, node_filter);
        for node in filtered.node_references() {
            let new_node = out_graph.add_node(node.1.clone_ref(py));
            node_map.insert(node.0, new_node);
            node_dict.insert(new_node.index(), node.0.index());
        }
        for edge in filtered.edge_references() {
            let new_source = *node_map.get(&edge.source()).unwrap();
            let new_target = *node_map.get(&edge.target()).unwrap();
            out_graph.add_edge(new_source, new_target, edge.weight().clone_ref(py));
        }
        let attrs = if preserve_attrs {
            self.attrs.clone_ref(py)
        } else {
            py.None()
        };
        let node_map = NodeMap {
            node_map: node_dict,
        };
        let subgraph = PyGraph {
            graph: out_graph,
            node_removed: false,
            multigraph: self.multigraph,
            attrs,
        };
        (subgraph, node_map)
    }

    /// Return a new PyGraph object for a subgraph of this graph.
    ///
    /// .. note::
    ///     To return a NodeMap object that maps the nodes of the subgraph to
    ///     the nodes of the original graph, use :meth:`.subgraph_with_nodemap()`.
    ///
    /// :param list[int] nodes: A list of node indices to generate the subgraph
    ///     from. If a node index is included that is not present in the graph
    ///     it will silently be ignored.
    /// :param bool preserve_attrs: If set to the True the attributes of the PyGraph
    ///     will be copied by reference to be the attributes of the output
    ///     subgraph. By default this is set to False and the :attr:`~.PyGraph.attrs`
    ///     attribute will be ``None`` in the subgraph.
    ///
    /// :returns: A new PyGraph object representing a subgraph of this graph.
    ///     It is worth noting that node and edge weight/data payloads are
    ///     passed by reference so if you update (not replace) an object used
    ///     as the weight in graph or the subgraph it will also be updated in
    ///     the other.
    /// :rtype: PyGraph
    ///
    #[pyo3(signature=(nodes, preserve_attrs=false), text_signature = "(self, nodes, /, preserve_attrs=False)")]
    pub fn subgraph(&self, py: Python, nodes: Vec<usize>, preserve_attrs: bool) -> PyGraph {
        let (subgraph, _) = self.subgraph_with_nodemap(py, nodes, preserve_attrs);
        subgraph
    }

    /// Return a new PyGraph object for an edge induced subgraph of this graph
    ///
    /// The induced subgraph contains each edge in `edge_list` and each node
    /// incident to any of those edges.
    ///
    /// :param list[tuple[int, int]] edge_list: A list of edge tuples (2-tuples with the source
    ///     and target node) to generate the subgraph from. In cases of parallel
    ///     edges for a multigraph all edges between the specified node. In case
    ///     of an edge specified that doesn't exist in the graph it will be
    ///     silently ignored.
    ///
    /// :returns: The edge subgraph
    /// :rtype: PyGraph
    ///
    #[pyo3(text_signature = "(self, edge_list, /)")]
    pub fn edge_subgraph(&self, edge_list: Vec<[usize; 2]>) -> PyGraph {
        // Filter non-existent edges
        let edges: Vec<[usize; 2]> = edge_list
            .into_iter()
            .filter(|x| {
                let source = NodeIndex::new(x[0]);
                let target = NodeIndex::new(x[1]);
                self.graph.find_edge(source, target).is_some()
            })
            .collect();

        let nodes: HashSet<NodeIndex> = edges
            .iter()
            .flat_map(|x| x.iter())
            .copied()
            .map(NodeIndex::new)
            .collect();
        let mut edge_set: HashSet<[NodeIndex; 2]> = HashSet::with_capacity(edges.len());
        for edge in edges {
            let source_index = NodeIndex::new(edge[0]);
            let target_index = NodeIndex::new(edge[1]);
            edge_set.insert([source_index, target_index]);
        }
        let mut out_graph = self.clone();
        for node in self
            .graph
            .node_indices()
            .filter(|node| !nodes.contains(node))
        {
            out_graph.graph.remove_node(node);
            out_graph.node_removed = true;
        }
        for edge in self.graph.edge_references().filter(|edge| {
            !edge_set.contains(&[edge.source(), edge.target()])
                && !edge_set.contains(&[edge.target(), edge.source()])
        }) {
            out_graph.graph.remove_edge(edge.id());
        }
        out_graph
    }

    /// Return a shallow copy of the graph
    ///
    /// All node and edge weight/data payloads in the copy will have a
    /// shared reference to the original graph.
    /// :returns: A shallow copy of the graph
    /// :rtype: PyGraph
    #[pyo3(text_signature = "(self)")]
    pub fn copy(&self) -> PyGraph {
        self.clone()
    }

    /// Return the number of nodes in the graph
    fn __len__(&self) -> PyResult<usize> {
        Ok(self.graph.node_count())
    }

    fn __getitem__(&self, idx: usize) -> PyResult<&PyObject> {
        match self.graph.node_weight(NodeIndex::new(idx)) {
            Some(data) => Ok(data),
            None => Err(PyIndexError::new_err("No node found for index")),
        }
    }

    fn __setitem__(&mut self, idx: usize, value: PyObject) -> PyResult<()> {
        let data = match self.graph.node_weight_mut(NodeIndex::new(idx)) {
            Some(node_data) => node_data,
            None => return Err(PyIndexError::new_err("No node found for index")),
        };
        *data = value;
        Ok(())
    }

    fn __delitem__(&mut self, idx: usize) -> PyResult<()> {
        match self.graph.remove_node(NodeIndex::new(idx)) {
            Some(_) => {
                self.node_removed = true;
                Ok(())
            }
            None => Err(PyIndexError::new_err("No node found for index")),
        }
    }

    #[classmethod]
    #[pyo3(signature = (key, /))]
    pub fn __class_getitem__(
        cls: &Bound<'_, PyType>,
        key: &Bound<'_, PyAny>,
    ) -> PyResult<PyObject> {
        let alias = PyGenericAlias::new(cls.py(), cls.as_any(), key)?;
        Ok(alias.into())
    }

    // Functions to enable Python Garbage Collection

    // Function for PyTypeObject.tp_traverse [1][2] used to tell Python what
    // objects the PyGraph has strong references to.
    //
    // [1] https://docs.python.org/3/c-api/typeobj.html#c.PyTypeObject.tp_traverse
    // [2] https://pyo3.rs/v0.12.4/class/protocols.html#garbage-collector-integration
    fn __traverse__(&self, visit: PyVisit) -> Result<(), PyTraverseError> {
        for node in self
            .graph
            .node_indices()
            .map(|node| self.graph.node_weight(node).unwrap())
        {
            visit.call(node)?;
        }
        for edge in self
            .graph
            .edge_indices()
            .map(|edge| self.graph.edge_weight(edge).unwrap())
        {
            visit.call(edge)?;
        }
        visit.call(&self.attrs)?;
        Ok(())
    }

    // Function for PyTypeObject.tp_clear [1][2] used to tell Python's GC how
    // to drop all references held by a PyGraph object when the GC needs to
    // break reference cycles.
    //
    // ]1] https://docs.python.org/3/c-api/typeobj.html#c.PyTypeObject.tp_clear
    // [2] https://pyo3.rs/v0.12.4/class/protocols.html#garbage-collector-integration
    fn __clear__(&mut self, py: Python) {
        self.graph = StablePyGraph::<Undirected>::default();
        self.node_removed = false;
        self.attrs = py.None();
    }

    /// Filters a graph's nodes by some criteria conditioned on a node's data payload and returns those nodes' indices.
    ///
    /// This function takes in a function as an argument. This filter function will be passed in a node's data payload and is
    /// required to return a boolean value stating whether the node's data payload fits some criteria.
    ///
    /// For example::
    ///
    ///     from rustworkx import PyGraph
    ///
    ///     graph = PyGraph()
    ///     graph.add_nodes_from(list(range(5)))
    ///
    ///     def my_filter_function(node):
    ///         return node > 2
    ///
    ///     indices = graph.filter_nodes(my_filter_function)
    ///     assert indices == [3, 4]
    ///
    /// :param Callable filter_function: Function to filter nodes with
    /// :returns: The node indices that match the filter
    /// :rtype: NodeIndices
    #[pyo3(text_signature = "(self, filter_function)")]
    pub fn filter_nodes(&self, py: Python, filter_function: PyObject) -> PyResult<NodeIndices> {
        let filter = |nindex: NodeIndex| -> PyResult<bool> {
            let res = filter_function.call1(py, (&self.graph[nindex],))?;
            res.extract(py)
        };

        let mut n = Vec::with_capacity(self.graph.node_count());
        for node_index in self.graph.node_indices() {
            if filter(node_index)? {
                n.push(node_index.index())
            };
        }
        Ok(NodeIndices { nodes: n })
    }

    /// Filters a graph's edges by some criteria conditioned on a edge's data payload and returns those edges' indices.
    ///
    /// This function takes in a function as an argument. This filter function will be passed in an edge's data payload and is
    /// required to return a boolean value stating whether the edge's data payload fits some criteria.
    ///
    /// For example::
    ///
    ///     from rustworkx import PyGraph
    ///     from rustworkx.generators import complete_graph
    ///
    ///     graph = PyGraph()
    ///     graph.add_nodes_from(range(3))
    ///     graph.add_edges_from([(0, 1, 'A'), (0, 1, 'B'), (1, 2, 'C')])
    ///
    ///     def my_filter_function(edge):
    ///         if edge:
    ///             return edge == 'B'
    ///         return False
    ///
    ///     indices = graph.filter_edges(my_filter_function)
    ///     assert indices == [1]
    ///
    /// :param Callable filter_function: Function to filter edges with
    /// :returns: The edge indices that match the filter
    /// :rtype: EdgeIndices
    #[pyo3(text_signature = "(self, filter_function)")]
    pub fn filter_edges(&self, py: Python, filter_function: PyObject) -> PyResult<EdgeIndices> {
        let filter = |eindex: EdgeIndex| -> PyResult<bool> {
            let res = filter_function.call1(py, (&self.graph[eindex],))?;
            res.extract(py)
        };

        let mut e = Vec::with_capacity(self.graph.edge_count());
        for edge_index in self.graph.edge_indices() {
            if filter(edge_index)? {
                e.push(edge_index.index())
            };
        }
        Ok(EdgeIndices { edges: e })
    }
}

fn weight_transform_callable(
    py: Python,
    map_fn: &Option<PyObject>,
    value: &PyObject,
) -> PyResult<PyObject> {
    match map_fn {
        Some(map_fn) => {
            let res = map_fn.call1(py, (value,))?;
            res.into_py_any(py)
        }
        None => Ok(value.clone_ref(py)),
    }
}

fn _from_adjacency_matrix<'p, T>(
    py: Python<'p>,
    matrix: PyReadonlyArray2<'p, T>,
    null_value: T,
) -> PyResult<PyGraph>
where
    T: Copy + std::cmp::PartialEq + numpy::Element + pyo3::IntoPyObject<'p> + IsNan,
{
    let array = matrix.as_array();
    let shape = array.shape();
    let mut out_graph = StablePyGraph::<Undirected>::default();
    let _node_indices: Vec<NodeIndex> = (0..shape[0])
        .map(|node| Ok(out_graph.add_node(node.into_py_any(py)?)))
        .collect::<PyResult<Vec<NodeIndex>>>()?;
    for (index, row) in array.axis_iter(Axis(0)).enumerate() {
        let source_index = NodeIndex::new(index);
        for (target_index, elem) in row.iter().enumerate() {
            if target_index < index {
                continue;
            }
            if null_value.is_nan() {
                if !elem.is_nan() {
                    out_graph.add_edge(
                        source_index,
                        NodeIndex::new(target_index),
                        elem.into_py_any(py)?,
                    );
                }
            } else if *elem != null_value {
                out_graph.add_edge(
                    source_index,
                    NodeIndex::new(target_index),
                    elem.into_py_any(py)?,
                );
            }
        }
    }
    Ok(PyGraph {
        graph: out_graph,
        node_removed: false,
        multigraph: true,
        attrs: py.None(),
    })
}
