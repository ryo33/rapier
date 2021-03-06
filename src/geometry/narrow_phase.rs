#[cfg(feature = "parallel")]
use rayon::prelude::*;

use crate::dynamics::RigidBodySet;
use crate::geometry::contact_generator::{
    ContactDispatcher, ContactGenerationContext, DefaultContactDispatcher,
};
use crate::geometry::proximity_detector::{
    DefaultProximityDispatcher, ProximityDetectionContext, ProximityDispatcher,
};
//#[cfg(feature = "simd-is-enabled")]
//use crate::geometry::{
//    contact_generator::ContactGenerationContextSimd,
//    proximity_detector::ProximityDetectionContextSimd, WBall,
//};
use crate::geometry::{
    BroadPhasePairEvent, ColliderGraphIndex, ColliderHandle, ContactEvent, ContactPairFilter,
    PairFilterContext, ProximityEvent, ProximityPair, ProximityPairFilter, RemovedCollider,
    SolverFlags,
};
use crate::geometry::{ColliderSet, ContactManifold, ContactPair, InteractionGraph};
//#[cfg(feature = "simd-is-enabled")]
//use crate::math::{SimdFloat, SIMD_WIDTH};
use crate::data::pubsub::Subscription;
use crate::ncollide::query::Proximity;
use crate::pipeline::EventHandler;
use std::collections::HashMap;
//use simba::simd::SimdValue;

/// The narrow-phase responsible for computing precise contact information between colliders.
#[cfg_attr(feature = "serde-serialize", derive(Serialize, Deserialize))]
#[derive(Clone)]
pub struct NarrowPhase {
    contact_graph: InteractionGraph<ContactPair>,
    proximity_graph: InteractionGraph<ProximityPair>,
    removed_colliders: Option<Subscription<RemovedCollider>>,
    //    ball_ball: Vec<usize>,        // Workspace: Vec<*mut ContactPair>,
    //    shape_shape: Vec<usize>,      // Workspace: Vec<*mut ContactPair>,
    //    ball_ball_prox: Vec<usize>,   // Workspace: Vec<*mut ProximityPair>,
    //    shape_shape_prox: Vec<usize>, // Workspace: Vec<*mut ProximityPair>,
}

pub(crate) type ContactManifoldIndex = usize;

impl NarrowPhase {
    /// Creates a new empty narrow-phase.
    pub fn new() -> Self {
        Self {
            contact_graph: InteractionGraph::new(),
            proximity_graph: InteractionGraph::new(),
            removed_colliders: None,
            //            ball_ball: Vec::new(),
            //            shape_shape: Vec::new(),
            //            ball_ball_prox: Vec::new(),
            //            shape_shape_prox: Vec::new(),
        }
    }

    /// The contact graph containing all contact pairs and their contact information.
    pub fn contact_graph(&self) -> &InteractionGraph<ContactPair> {
        &self.contact_graph
    }

    /// The proximity graph containing all proximity pairs and their proximity information.
    pub fn proximity_graph(&self) -> &InteractionGraph<ProximityPair> {
        &self.proximity_graph
    }

    // #[cfg(feature = "parallel")]
    // pub fn contact_pairs(&self) -> &[ContactPair] {
    //     &self.contact_graph.interactions
    // }

    // pub fn contact_pairs_mut(&mut self) -> &mut [ContactPair] {
    //     &mut self.contact_graph.interactions
    // }

    // #[cfg(feature = "parallel")]
    // pub(crate) fn contact_pairs_vec_mut(&mut self) -> &mut Vec<ContactPair> {
    //     &mut self.contact_graph.interactions
    // }

    /// Maintain the narrow-phase internal state by taking collider removal into account.
    pub fn maintain(&mut self, colliders: &mut ColliderSet, bodies: &mut RigidBodySet) {
        // Ensure we already subscribed.
        if self.removed_colliders.is_none() {
            self.removed_colliders = Some(colliders.removed_colliders.subscribe());
        }

        let mut cursor = self.removed_colliders.take().unwrap();

        // TODO: avoid these hash-maps.
        // They are necessary to handle the swap-remove done internally
        // by the contact/proximity graphs when a node is removed.
        let mut prox_id_remap = HashMap::new();
        let mut contact_id_remap = HashMap::new();

        for i in 0.. {
            if let Some(collider) = colliders.removed_colliders.read_ith(&cursor, i) {
                let proximity_graph_id = prox_id_remap
                    .get(&collider.handle)
                    .copied()
                    .unwrap_or(collider.proximity_graph_index);
                let contact_graph_id = contact_id_remap
                    .get(&collider.handle)
                    .copied()
                    .unwrap_or(collider.contact_graph_index);

                self.remove_collider(
                    proximity_graph_id,
                    contact_graph_id,
                    colliders,
                    bodies,
                    &mut prox_id_remap,
                    &mut contact_id_remap,
                );
            } else {
                break;
            }
        }

        colliders.removed_colliders.ack(&mut cursor);
        self.removed_colliders = Some(cursor);
    }

    pub(crate) fn remove_collider<'a>(
        &mut self,
        proximity_graph_id: ColliderGraphIndex,
        contact_graph_id: ColliderGraphIndex,
        colliders: &mut ColliderSet,
        bodies: &mut RigidBodySet,
        prox_id_remap: &mut HashMap<ColliderHandle, ColliderGraphIndex>,
        contact_id_remap: &mut HashMap<ColliderHandle, ColliderGraphIndex>,
    ) {
        // Wake up every body in contact with the deleted collider.
        for (a, b, _) in self.contact_graph.interactions_with(contact_graph_id) {
            if let Some(parent) = colliders.get(a).map(|c| c.parent) {
                bodies.wake_up(parent, true)
            }

            if let Some(parent) = colliders.get(b).map(|c| c.parent) {
                bodies.wake_up(parent, true)
            }
        }

        // We have to manage the fact that one other collider will
        // have its graph index changed because of the node's swap-remove.
        if let Some(replacement) = self.proximity_graph.remove_node(proximity_graph_id) {
            if let Some(replacement) = colliders.get_mut(replacement) {
                replacement.proximity_graph_index = proximity_graph_id;
            } else {
                prox_id_remap.insert(replacement, proximity_graph_id);
            }
        }

        if let Some(replacement) = self.contact_graph.remove_node(contact_graph_id) {
            if let Some(replacement) = colliders.get_mut(replacement) {
                replacement.contact_graph_index = contact_graph_id;
            } else {
                contact_id_remap.insert(replacement, contact_graph_id);
            }
        }
    }

    pub(crate) fn register_pairs(
        &mut self,
        colliders: &mut ColliderSet,
        bodies: &mut RigidBodySet,
        broad_phase_events: &[BroadPhasePairEvent],
        events: &dyn EventHandler,
    ) {
        for event in broad_phase_events {
            match event {
                BroadPhasePairEvent::AddPair(pair) => {
                    // println!("Adding pair: {:?}", *pair);
                    if let (Some(co1), Some(co2)) =
                        colliders.get2_mut_internal(pair.collider1, pair.collider2)
                    {
                        if co1.parent == co2.parent {
                            // Same parents. Ignore collisions.
                            continue;
                        }

                        if co1.is_sensor() || co2.is_sensor() {
                            let gid1 = co1.proximity_graph_index;
                            let gid2 = co2.proximity_graph_index;

                            // NOTE: the collider won't have a graph index as long
                            // as it does not interact with anything.
                            if !InteractionGraph::<ProximityPair>::is_graph_index_valid(gid1) {
                                co1.proximity_graph_index =
                                    self.proximity_graph.graph.add_node(pair.collider1);
                            }

                            if !InteractionGraph::<ProximityPair>::is_graph_index_valid(gid2) {
                                co2.proximity_graph_index =
                                    self.proximity_graph.graph.add_node(pair.collider2);
                            }

                            if self.proximity_graph.graph.find_edge(gid1, gid2).is_none() {
                                let dispatcher = DefaultProximityDispatcher;
                                let generator = dispatcher
                                    .dispatch(co1.shape().shape_type(), co2.shape().shape_type());
                                let interaction =
                                    ProximityPair::new(*pair, generator.0, generator.1);
                                let _ = self.proximity_graph.add_edge(
                                    co1.proximity_graph_index,
                                    co2.proximity_graph_index,
                                    interaction,
                                );
                            }
                        } else {
                            // NOTE: same code as above, but for the contact graph.
                            // TODO: refactor both pieces of code somehow?
                            let gid1 = co1.contact_graph_index;
                            let gid2 = co2.contact_graph_index;

                            // NOTE: the collider won't have a graph index as long
                            // as it does not interact with anything.
                            if !InteractionGraph::<ContactPair>::is_graph_index_valid(gid1) {
                                co1.contact_graph_index =
                                    self.contact_graph.graph.add_node(pair.collider1);
                            }

                            if !InteractionGraph::<ContactPair>::is_graph_index_valid(gid2) {
                                co2.contact_graph_index =
                                    self.contact_graph.graph.add_node(pair.collider2);
                            }

                            if self.contact_graph.graph.find_edge(gid1, gid2).is_none() {
                                let dispatcher = DefaultContactDispatcher;
                                let generator = dispatcher
                                    .dispatch(co1.shape().shape_type(), co2.shape().shape_type());
                                let interaction = ContactPair::new(*pair, generator.0, generator.1);
                                let _ = self.contact_graph.add_edge(
                                    co1.contact_graph_index,
                                    co2.contact_graph_index,
                                    interaction,
                                );
                            }
                        }
                    }
                }
                BroadPhasePairEvent::DeletePair(pair) => {
                    if let (Some(co1), Some(co2)) =
                        colliders.get2_mut_internal(pair.collider1, pair.collider2)
                    {
                        if co1.is_sensor() || co2.is_sensor() {
                            let prox_pair = self
                                .proximity_graph
                                .remove_edge(co1.proximity_graph_index, co2.proximity_graph_index);

                            // Emit a proximity lost event if we had a proximity before removing the edge.
                            if let Some(prox) = prox_pair {
                                if prox.proximity != Proximity::Disjoint {
                                    let prox_event = ProximityEvent::new(
                                        pair.collider1,
                                        pair.collider2,
                                        prox.proximity,
                                        Proximity::Disjoint,
                                    );
                                    events.handle_proximity_event(prox_event)
                                }
                            }
                        } else {
                            let contact_pair = self
                                .contact_graph
                                .remove_edge(co1.contact_graph_index, co2.contact_graph_index);

                            // Emit a contact stopped event if we had a contact before removing the edge.
                            // Also wake up the dynamic bodies that were in contact.
                            if let Some(ctct) = contact_pair {
                                if ctct.has_any_active_contact() {
                                    bodies.wake_up(co1.parent, true);
                                    bodies.wake_up(co2.parent, true);

                                    events.handle_contact_event(ContactEvent::Stopped(
                                        pair.collider1,
                                        pair.collider2,
                                    ))
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    pub(crate) fn compute_proximities(
        &mut self,
        prediction_distance: f32,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
        pair_filter: Option<&dyn ProximityPairFilter>,
        events: &dyn EventHandler,
    ) {
        par_iter_mut!(&mut self.proximity_graph.graph.edges).for_each(|edge| {
            let pair = &mut edge.weight;
            let co1 = &colliders[pair.pair.collider1];
            let co2 = &colliders[pair.pair.collider2];

            // FIXME: avoid lookup into bodies.
            let rb1 = &bodies[co1.parent];
            let rb2 = &bodies[co2.parent];

            if (rb1.is_sleeping() && rb2.is_static())
                || (rb2.is_sleeping() && rb1.is_static())
                || (rb1.is_sleeping() && rb2.is_sleeping())
            {
                // No need to update this proximity because nothing moved.
                return;
            }

            if !co1.collision_groups.test(co2.collision_groups) {
                // The proximity is not allowed.
                return;
            }

            if pair_filter.is_none() && !rb1.is_dynamic() && !rb2.is_dynamic() {
                // Default filtering rule: no proximity between two non-dynamic bodies.
                return;
            }

            if let Some(filter) = pair_filter {
                let context = PairFilterContext {
                    rigid_body1: rb1,
                    rigid_body2: rb2,
                    collider1: co1,
                    collider2: co2,
                };

                if !filter.filter_proximity_pair(&context) {
                    // No proximity allowed.
                    return;
                }
            }

            let dispatcher = DefaultProximityDispatcher;
            if pair.detector.is_none() {
                // We need a redispatch for this detector.
                // This can happen, e.g., after restoring a snapshot of the narrow-phase.
                let (detector, workspace) =
                    dispatcher.dispatch(co1.shape().shape_type(), co2.shape().shape_type());
                pair.detector = Some(detector);
                pair.detector_workspace = workspace;
            }

            let context = ProximityDetectionContext {
                dispatcher: &dispatcher,
                prediction_distance,
                colliders,
                pair,
            };

            context
                .pair
                .detector
                .unwrap()
                .detect_proximity(context, events);
        });
    }

    pub(crate) fn compute_contacts(
        &mut self,
        prediction_distance: f32,
        bodies: &RigidBodySet,
        colliders: &ColliderSet,
        pair_filter: Option<&dyn ContactPairFilter>,
        events: &dyn EventHandler,
    ) {
        par_iter_mut!(&mut self.contact_graph.graph.edges).for_each(|edge| {
            let pair = &mut edge.weight;
            let co1 = &colliders[pair.pair.collider1];
            let co2 = &colliders[pair.pair.collider2];

            // FIXME: avoid lookup into bodies.
            let rb1 = &bodies[co1.parent];
            let rb2 = &bodies[co2.parent];

            if (rb1.is_sleeping() && rb2.is_static())
                || (rb2.is_sleeping() && rb1.is_static())
                || (rb1.is_sleeping() && rb2.is_sleeping())
            {
                // No need to update this contact because nothing moved.
                return;
            }

            if !co1.collision_groups.test(co2.collision_groups) {
                // The collision is not allowed.
                return;
            }

            if pair_filter.is_none() && !rb1.is_dynamic() && !rb2.is_dynamic() {
                // Default filtering rule: no contact between two non-dynamic bodies.
                return;
            }

            let mut solver_flags = if let Some(filter) = pair_filter {
                let context = PairFilterContext {
                    rigid_body1: rb1,
                    rigid_body2: rb2,
                    collider1: co1,
                    collider2: co2,
                };

                if let Some(solver_flags) = filter.filter_contact_pair(&context) {
                    solver_flags
                } else {
                    // No contact allowed.
                    return;
                }
            } else {
                SolverFlags::COMPUTE_IMPULSES
            };

            if !co1.solver_groups.test(co2.solver_groups) {
                solver_flags.remove(SolverFlags::COMPUTE_IMPULSES);
            }

            let dispatcher = DefaultContactDispatcher;
            if pair.generator.is_none() {
                // We need a redispatch for this generator.
                // This can happen, e.g., after restoring a snapshot of the narrow-phase.
                let (generator, workspace) =
                    dispatcher.dispatch(co1.shape().shape_type(), co2.shape().shape_type());
                pair.generator = Some(generator);

                // Keep the workspace if one already exists.
                if pair.generator_workspace.is_none() {
                    pair.generator_workspace = workspace;
                }
            }

            let context = ContactGenerationContext {
                dispatcher: &dispatcher,
                prediction_distance,
                colliders,
                pair,
                solver_flags,
            };

            context
                .pair
                .generator
                .unwrap()
                .generate_contacts(context, events);
        });
    }

    /// Retrieve all the interactions with at least one contact point, happening between two active bodies.
    // NOTE: this is very similar to the code from JointSet::select_active_interactions.
    pub(crate) fn sort_and_select_active_contacts<'a>(
        &'a mut self,
        bodies: &RigidBodySet,
        out_manifolds: &mut Vec<&'a mut ContactManifold>,
        out: &mut Vec<Vec<ContactManifoldIndex>>,
    ) {
        for out_island in &mut out[..bodies.num_islands()] {
            out_island.clear();
        }

        // FIXME: don't iterate through all the interactions.
        for inter in self.contact_graph.graph.edges.iter_mut() {
            for manifold in &mut inter.weight.manifolds {
                let rb1 = &bodies[manifold.body_pair.body1];
                let rb2 = &bodies[manifold.body_pair.body2];
                if manifold
                    .solver_flags
                    .contains(SolverFlags::COMPUTE_IMPULSES)
                    && manifold.num_active_contacts() != 0
                    && (rb1.is_dynamic() || rb2.is_dynamic())
                    && (!rb1.is_dynamic() || !rb1.is_sleeping())
                    && (!rb2.is_dynamic() || !rb2.is_sleeping())
                {
                    let island_index = if !rb1.is_dynamic() {
                        rb2.active_island_id
                    } else {
                        rb1.active_island_id
                    };

                    out[island_index].push(out_manifolds.len());
                    out_manifolds.push(manifold);
                }
            }
        }
    }
}
