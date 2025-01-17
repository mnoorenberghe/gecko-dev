/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{ClipId, DeviceIntRect, LayerPixel, LayerPoint, LayerRect, LayerSize};
use api::{LayerToScrollTransform, LayerToWorldTransform, LayerVector2D, PipelineId};
use api::{ScrollClamping, ScrollEventPhase, ScrollLocation, ScrollSensitivity, StickyOffsetBounds};
use api::WorldPoint;
use clip::{ClipRegion, ClipSources, ClipSourcesHandle, ClipStore};
use clip_scroll_tree::{CoordinateSystemId, TransformUpdateState};
use euclid::SideOffsets2D;
use geometry::ray_intersects_rect;
use gpu_cache::GpuCache;
use render_task::{ClipChain, ClipChainNode, ClipWorkItem};
use resource_cache::ResourceCache;
use spring::{DAMPING, STIFFNESS, Spring};
use std::rc::Rc;
use tiling::{PackedLayer, PackedLayerIndex};
use util::{MatrixHelpers, MaxRect};

#[cfg(target_os = "macos")]
const CAN_OVERSCROLL: bool = true;

#[cfg(not(target_os = "macos"))]
const CAN_OVERSCROLL: bool = false;

#[derive(Debug)]
pub struct ClipInfo {
    /// The clips for this node.
    pub clip_sources: ClipSourcesHandle,

    /// The packed layer index for this node, which is used to render a clip mask
    /// for it, if necessary.
    pub packed_layer_index: PackedLayerIndex,

    /// Whether or not this clip node automatically creates a mask.
    pub is_masking: bool,
}

impl ClipInfo {
    pub fn new(
        clip_region: ClipRegion,
        packed_layer_index: PackedLayerIndex,
        clip_store: &mut ClipStore,
    ) -> ClipInfo {
        let clip_sources = ClipSources::from(clip_region);
        let is_masking = clip_sources.is_masking();

        ClipInfo {
            clip_sources: clip_store.insert(clip_sources),
            packed_layer_index,
            is_masking,
        }
    }
}

#[derive(Debug)]
pub struct StickyFrameInfo {
    pub margins: SideOffsets2D<Option<f32>>,
    pub vertical_offset_bounds: StickyOffsetBounds,
    pub horizontal_offset_bounds: StickyOffsetBounds,
    pub current_offset: LayerVector2D,
}

impl StickyFrameInfo {
    pub fn new(
        margins: SideOffsets2D<Option<f32>>,
        vertical_offset_bounds: StickyOffsetBounds,
        horizontal_offset_bounds: StickyOffsetBounds
    ) -> StickyFrameInfo {
        StickyFrameInfo {
            margins,
            vertical_offset_bounds,
            horizontal_offset_bounds,
            current_offset: LayerVector2D::zero(),
        }
    }
}

#[derive(Debug)]
pub enum NodeType {
    /// A reference frame establishes a new coordinate space in the tree.
    ReferenceFrame(ReferenceFrameInfo),

    /// Other nodes just do clipping, but no transformation.
    Clip(ClipInfo),

    /// Transforms it's content, but doesn't clip it. Can also be adjusted
    /// by scroll events or setting scroll offsets.
    ScrollFrame(ScrollingState),

    /// A special kind of node that adjusts its position based on the position
    /// of its parent node and a given set of sticky positioning offset bounds.
    /// Sticky positioned is described in the CSS Positioned Layout Module Level 3 here:
    /// https://www.w3.org/TR/css-position-3/#sticky-pos
    StickyFrame(StickyFrameInfo),
}

/// Contains information common among all types of ClipScrollTree nodes.
#[derive(Debug)]
pub struct ClipScrollNode {
    /// Viewing rectangle in the coordinate system of the parent reference frame.
    pub local_viewport_rect: LayerRect,

    /// Clip rect of this node - typically the same as viewport rect, except
    /// in overscroll cases.
    pub local_clip_rect: LayerRect,

    /// Viewport rectangle clipped against parent layer(s) viewport rectangles.
    /// This is in the coordinate system of the node origin.
    /// Precisely, it combines the local clipping rectangles of all the parent
    /// nodes on the way to the root, including those of `ClipRegion` rectangles.
    /// The combined clip is lossy/concervative on `ReferenceFrame` nodes.
    pub combined_local_viewport_rect: LayerRect,

    /// World transform for the viewport rect itself. This is the parent
    /// reference frame transformation plus the scrolling offsets provided by
    /// the nodes in between the reference frame and this node.
    pub world_viewport_transform: LayerToWorldTransform,

    /// World transform for content transformed by this node.
    pub world_content_transform: LayerToWorldTransform,

    /// The scroll offset of all the nodes between us and our parent reference frame.
    /// This is used to calculate intersections between us and content or nodes that
    /// are also direct children of our reference frame.
    pub reference_frame_relative_scroll_offset: LayerVector2D,

    /// Pipeline that this layer belongs to
    pub pipeline_id: PipelineId,

    /// Parent layer. If this is None, we are the root node.
    pub parent: Option<ClipId>,

    /// Child layers
    pub children: Vec<ClipId>,

    /// Whether or not this node is a reference frame.
    pub node_type: NodeType,

    /// The node in the chain of clips that are necessary to clip display items
    /// that have this ClipScrollNode as their clip parent. This will be used to
    /// generate clip tasks.
    pub clip_chain_node: ClipChain,

    /// The intersected outer bounds of the clips for this node.
    pub combined_clip_outer_bounds: DeviceIntRect,

    /// The axis-aligned coordinate system id of this node.
    pub coordinate_system_id: CoordinateSystemId,
}

impl ClipScrollNode {
    fn new(
        pipeline_id: PipelineId,
        parent_id: Option<ClipId>,
        rect: &LayerRect,
        node_type: NodeType
    ) -> ClipScrollNode {
        ClipScrollNode {
            local_viewport_rect: *rect,
            local_clip_rect: *rect,
            combined_local_viewport_rect: LayerRect::zero(),
            world_viewport_transform: LayerToWorldTransform::identity(),
            world_content_transform: LayerToWorldTransform::identity(),
            reference_frame_relative_scroll_offset: LayerVector2D::zero(),
            parent: parent_id,
            children: Vec::new(),
            pipeline_id,
            node_type: node_type,
            clip_chain_node: None,
            combined_clip_outer_bounds: DeviceIntRect::max_rect(),
            coordinate_system_id: CoordinateSystemId(0),
        }
    }

    pub fn new_scroll_frame(
        pipeline_id: PipelineId,
        parent_id: ClipId,
        frame_rect: &LayerRect,
        content_size: &LayerSize,
        scroll_sensitivity: ScrollSensitivity,
    ) -> ClipScrollNode {
        let node_type = NodeType::ScrollFrame(ScrollingState::new(
            scroll_sensitivity,
            LayerSize::new(
                (content_size.width - frame_rect.size.width).max(0.0),
                (content_size.height - frame_rect.size.height).max(0.0)
            )
        ));

        Self::new(pipeline_id, Some(parent_id), frame_rect, node_type)
    }

    pub fn new_clip_node(
        pipeline_id: PipelineId,
        parent_id: ClipId,
        clip_info: ClipInfo,
        clip_rect: LayerRect,
    ) -> ClipScrollNode {
        Self::new(pipeline_id, Some(parent_id), &clip_rect, NodeType::Clip(clip_info))
    }

    pub fn new_reference_frame(
        parent_id: Option<ClipId>,
        frame_rect: &LayerRect,
        transform: &LayerToScrollTransform,
        origin_in_parent_reference_frame: LayerVector2D,
        pipeline_id: PipelineId,
    ) -> ClipScrollNode {
        let info = ReferenceFrameInfo {
            transform: *transform,
            origin_in_parent_reference_frame,
        };
        Self::new(pipeline_id, parent_id, frame_rect, NodeType::ReferenceFrame(info))
    }

    pub fn new_sticky_frame(
        parent_id: ClipId,
        frame_rect: LayerRect,
        sticky_frame_info: StickyFrameInfo,
        pipeline_id: PipelineId,
    ) -> ClipScrollNode {
        let node_type = NodeType::StickyFrame(sticky_frame_info);
        Self::new(pipeline_id, Some(parent_id), &frame_rect, node_type)
    }


    pub fn add_child(&mut self, child: ClipId) {
        self.children.push(child);
    }

    pub fn apply_old_scrolling_state(&mut self, new_scrolling: &ScrollingState) {
        match self.node_type {
            NodeType::ScrollFrame(ref mut scrolling) => {
                let scroll_sensitivity = scrolling.scroll_sensitivity;
                *scrolling = *new_scrolling;
                scrolling.scroll_sensitivity = scroll_sensitivity;
            }
            _ if new_scrolling.offset != LayerVector2D::zero() => {
                warn!("Tried to scroll a non-scroll node.")
            }
            _ => {}
        }
    }

    pub fn set_scroll_origin(&mut self, origin: &LayerPoint, clamp: ScrollClamping) -> bool {
        let scrollable_size = self.scrollable_size();
        let scrollable_width = scrollable_size.width;
        let scrollable_height = scrollable_size.height;

        let scrolling = match self.node_type {
            NodeType::ScrollFrame(ref mut scrolling) => scrolling,
            _ => {
                warn!("Tried to scroll a non-scroll node.");
                return false;
            }
        };

        let new_offset = match clamp {
            ScrollClamping::ToContentBounds => {
                if scrollable_height <= 0. && scrollable_width <= 0. {
                    return false;
                }

                let origin = LayerPoint::new(origin.x.max(0.0), origin.y.max(0.0));
                LayerVector2D::new(
                    (-origin.x).max(-scrollable_width).min(0.0).round(),
                    (-origin.y).max(-scrollable_height).min(0.0).round(),
                )
            }
            ScrollClamping::NoClamping => LayerPoint::zero() - *origin,
        };

        if new_offset == scrolling.offset {
            return false;
        }

        scrolling.offset = new_offset;
        scrolling.bouncing_back = false;
        scrolling.started_bouncing_back = false;
        true
    }

    pub fn update_clip_work_item(
        &mut self,
        state: &mut TransformUpdateState,
        screen_rect: &DeviceIntRect,
        device_pixel_ratio: f32,
        packed_layers: &mut Vec<PackedLayer>,
        clip_store: &mut ClipStore,
        resource_cache: &mut ResourceCache,
        gpu_cache: &mut GpuCache,
    ) {
        self.coordinate_system_id = state.current_coordinate_system_id;

        let current_clip_chain = state.parent_clip_chain.clone();
        let clip_info = match self.node_type {
            NodeType::Clip(ref mut info) if info.is_masking => info,
            _ => {
                self.clip_chain_node = current_clip_chain;
                self.combined_clip_outer_bounds = state.combined_outer_clip_bounds;
                return;
            }
        };

        // The coordinates of the mask are relative to the origin of the node itself,
        // so we need to account for that origin in the transformation we assign to
        // the packed layer.
        let transform = self.world_viewport_transform
            .pre_translate(self.local_viewport_rect.origin.to_vector().to_3d());

        let packed_layer = &mut packed_layers[clip_info.packed_layer_index.0];
        if packed_layer.set_transform(transform) {
            // Meanwhile, the combined viewport rect is relative to the reference frame, so
            // we move it into the local coordinate system of the node.
            let local_viewport_rect = self.combined_local_viewport_rect
                .translate(&-self.local_viewport_rect.origin.to_vector());

            packed_layer.set_rect(
                &local_viewport_rect,
                screen_rect,
                device_pixel_ratio,
            );
        }

        let clip_sources = clip_store.get_mut(&clip_info.clip_sources);
        clip_sources.update(
            &transform,
            gpu_cache,
            resource_cache,
            device_pixel_ratio,
        );

        let outer_bounds = clip_sources.bounds.outer.as_ref().map_or_else(
            DeviceIntRect::zero,
            |rect| rect.device_rect
        );

        self.combined_clip_outer_bounds = outer_bounds.intersection(
            &state.combined_outer_clip_bounds).unwrap_or_else(DeviceIntRect::zero);

        // TODO: Combine rectangles in the same axis-aligned clip space here?
        self.clip_chain_node = Some(Rc::new(ClipChainNode {
            work_item: ClipWorkItem {
                layer_index: clip_info.packed_layer_index,
                clip_sources: clip_info.clip_sources.weak(),
                coordinate_system_id: state.current_coordinate_system_id,
            },
            prev: current_clip_chain,
        }));

        state.combined_outer_clip_bounds = self.combined_clip_outer_bounds;
        state.parent_clip_chain = self.clip_chain_node.clone();
    }

    pub fn update_transform(&mut self, state: &mut TransformUpdateState) {
        // We calculate this here to avoid a double-borrow later.
        let sticky_offset = self.calculate_sticky_offset(
            &state.nearest_scrolling_ancestor_offset,
            &state.nearest_scrolling_ancestor_viewport,
        );

        let (local_transform, accumulated_scroll_offset) = match self.node_type {
            NodeType::ReferenceFrame(ref info) => {
                self.combined_local_viewport_rect = info.transform
                    .with_destination::<LayerPixel>()
                    .inverse_rect_footprint(&state.parent_combined_viewport_rect);
                self.reference_frame_relative_scroll_offset = LayerVector2D::zero();
                (info.transform, state.parent_accumulated_scroll_offset)
            }
            NodeType::Clip(_) | NodeType::ScrollFrame(_) => {
                // Move the parent's viewport into the local space (of the node origin)
                // and intersect with the local clip rectangle to get the local viewport.
                self.combined_local_viewport_rect =
                    state.parent_combined_viewport_rect
                    .intersection(&self.local_clip_rect)
                    .unwrap_or(LayerRect::zero());
                self.reference_frame_relative_scroll_offset =
                    state.parent_accumulated_scroll_offset;
                (
                    LayerToScrollTransform::identity(),
                    self.reference_frame_relative_scroll_offset,
                )
            }
            NodeType::StickyFrame(ref mut info) => {
                info.current_offset = sticky_offset;
                self.combined_local_viewport_rect =
                    state.parent_combined_viewport_rect
                    .translate(&-sticky_offset)
                    .intersection(&self.local_clip_rect)
                    .unwrap_or(LayerRect::zero());
                self.reference_frame_relative_scroll_offset =
                    state.parent_accumulated_scroll_offset + sticky_offset;
                (LayerToScrollTransform::identity(), self.reference_frame_relative_scroll_offset)
            }
        };

        // The transformation for this viewport in world coordinates is the transformation for
        // our parent reference frame, plus any accumulated scrolling offsets from nodes
        // between our reference frame and this node. For reference frames, we also include
        // whatever local transformation this reference frame provides. This can be combined
        // with the local_viewport_rect to get its position in world space.
        self.world_viewport_transform = state
            .parent_reference_frame_transform
            .pre_translate(accumulated_scroll_offset.to_3d())
            .pre_mul(&local_transform.with_destination::<LayerPixel>());

        // The transformation for any content inside of us is the viewport transformation, plus
        // whatever scrolling offset we supply as well.
        let scroll_offset = self.scroll_offset();
        self.world_content_transform = self.world_viewport_transform
            .pre_translate(scroll_offset.to_3d());

        // The transformation we are passing is the transformation of the parent
        // reference frame and the offset is the accumulated offset of all the nodes
        // between us and the parent reference frame. If we are a reference frame,
        // we need to reset both these values.
        match self.node_type {
            NodeType::ReferenceFrame(ref info) => {
                state.parent_reference_frame_transform = self.world_viewport_transform;
                state.parent_combined_viewport_rect = self.combined_local_viewport_rect;
                state.parent_accumulated_scroll_offset = LayerVector2D::zero();
                state.nearest_scrolling_ancestor_viewport =
                    state.nearest_scrolling_ancestor_viewport
                       .translate(&info.origin_in_parent_reference_frame);

                if !info.transform.preserves_2d_axis_alignment() {
                    state.current_coordinate_system_id = state.next_coordinate_system_id;
                    state.next_coordinate_system_id = state.next_coordinate_system_id.next();
                }
            },
            NodeType::Clip(..) => {
                state.parent_combined_viewport_rect = self.combined_local_viewport_rect;
            },
            NodeType::ScrollFrame(ref scrolling) => {
                state.parent_combined_viewport_rect =
                        self.combined_local_viewport_rect.translate(&-scrolling.offset);
                state.parent_accumulated_scroll_offset =
                    scrolling.offset + state.parent_accumulated_scroll_offset;
                state.nearest_scrolling_ancestor_offset = scrolling.offset;
                state.nearest_scrolling_ancestor_viewport = self.local_viewport_rect;
            }
            NodeType::StickyFrame(ref info) => {
                // We don't translate the combined rect by the sticky offset, because sticky
                // offsets actually adjust the node position itself, whereas scroll offsets
                // only apply to contents inside the node.
                state.parent_combined_viewport_rect = self.combined_local_viewport_rect;
                state.parent_accumulated_scroll_offset =
                    info.current_offset + state.parent_accumulated_scroll_offset;
            }
        }
    }

    fn calculate_sticky_offset(
        &self,
        viewport_scroll_offset: &LayerVector2D,
        viewport_rect: &LayerRect,
    ) -> LayerVector2D {
        let info = match self.node_type {
            NodeType::StickyFrame(ref info) => info,
            _ => return LayerVector2D::zero(),
        };

        if info.margins.top.is_none() && info.margins.bottom.is_none() &&
            info.margins.left.is_none() && info.margins.right.is_none() {
            return LayerVector2D::zero();
        }

        // The viewport and margins of the item establishes the maximum amount that it can
        // be offset in order to keep it on screen. Since we care about the relationship
        // between the scrolled content and unscrolled viewport we adjust the viewport's
        // position by the scroll offset in order to work with their relative positions on the
        // page.
        let sticky_rect = self.local_viewport_rect.translate(viewport_scroll_offset);

        let mut sticky_offset = LayerVector2D::zero();
        if let Some(margin) = info.margins.top {
            // If the sticky rect is positioned above the top edge of the viewport (plus margin)
            // we move it down so that it is fully inside the viewport.
            let top_viewport_edge = viewport_rect.min_y() + margin;
            if sticky_rect.min_y() < top_viewport_edge {
                 sticky_offset.y = top_viewport_edge - sticky_rect.min_y();
            }
            debug_assert!(sticky_offset.y >= 0.0);
        }

        if sticky_offset.y == 0.0 {
            if let Some(margin) = info.margins.bottom {
                // If the bottom of the sticky rect is positioned below the bottom viewport edge
                // (accounting for margin), we move it up so that it is fully inside the viewport.
                let bottom_viewport_edge = viewport_rect.max_y() - margin;
                if sticky_rect.max_y() > bottom_viewport_edge {
                     sticky_offset.y = bottom_viewport_edge - sticky_rect.max_y();
                }
                debug_assert!(sticky_offset.y <= 0.0);
            }
        }

        if let Some(margin) = info.margins.left {
            // If the sticky rect is positioned left of the left edge of the viewport (plus margin)
            // we move it right so that it is fully inside the viewport.
            let left_viewport_edge = viewport_rect.min_x() + margin;
            if sticky_rect.min_x() < left_viewport_edge {
                 sticky_offset.x = left_viewport_edge - sticky_rect.min_x();
            }
            debug_assert!(sticky_offset.x >= 0.0);
        }

        if sticky_offset.x == 0.0 {
            if let Some(margin) = info.margins.right {
                // If the right edge of the sticky rect is positioned right of the right viewport
                // edge (accounting for margin), we move it left so that it is fully inside the
                // viewport.
                let right_viewport_edge = viewport_rect.max_x() - margin;
                if sticky_rect.max_x() > right_viewport_edge {
                     sticky_offset.x = right_viewport_edge - sticky_rect.max_x();
                }
                debug_assert!(sticky_offset.x <= 0.0);
            }
        }

        sticky_offset.y = sticky_offset.y.max(info.vertical_offset_bounds.min);
        sticky_offset.y = sticky_offset.y.min(info.vertical_offset_bounds.max);
        sticky_offset.x = sticky_offset.x.max(info.horizontal_offset_bounds.min);
        sticky_offset.x = sticky_offset.x.min(info.horizontal_offset_bounds.max);

        sticky_offset
    }

    pub fn scrollable_size(&self) -> LayerSize {
        match self.node_type {
           NodeType:: ScrollFrame(state) => state.scrollable_size,
            _ => LayerSize::zero(),
        }
    }


    pub fn scroll(&mut self, scroll_location: ScrollLocation, phase: ScrollEventPhase) -> bool {
        let scrolling = match self.node_type {
            NodeType::ScrollFrame(ref mut scrolling) => scrolling,
            _ => return false,
        };

        if scrolling.started_bouncing_back && phase == ScrollEventPhase::Move(false) {
            return false;
        }

        let mut delta = match scroll_location {
            ScrollLocation::Delta(delta) => delta,
            ScrollLocation::Start => {
                if scrolling.offset.y.round() >= 0.0 {
                    // Nothing to do on this layer.
                    return false;
                }

                scrolling.offset.y = 0.0;
                return true;
            }
            ScrollLocation::End => {
                let end_pos = -scrolling.scrollable_size.height;
                if scrolling.offset.y.round() <= end_pos {
                    // Nothing to do on this layer.
                    return false;
                }

                scrolling.offset.y = end_pos;
                return true;
            }
        };

        let overscroll_amount = scrolling.overscroll_amount();
        let overscrolling = CAN_OVERSCROLL && (overscroll_amount != LayerVector2D::zero());
        if overscrolling {
            if overscroll_amount.x != 0.0 {
                delta.x /= overscroll_amount.x.abs()
            }
            if overscroll_amount.y != 0.0 {
                delta.y /= overscroll_amount.y.abs()
            }
        }

        let scrollable_width = scrolling.scrollable_size.width;
        let scrollable_height = scrolling.scrollable_size.height;
        let is_unscrollable = scrollable_width <= 0. && scrollable_height <= 0.;
        let original_layer_scroll_offset = scrolling.offset;

        if scrollable_width > 0. {
            scrolling.offset.x = scrolling.offset.x + delta.x;
            if is_unscrollable || !CAN_OVERSCROLL {
                scrolling.offset.x = scrolling.offset.x.min(0.0).max(-scrollable_width).round();
            }
        }

        if scrollable_height > 0. {
            scrolling.offset.y = scrolling.offset.y + delta.y;
            if is_unscrollable || !CAN_OVERSCROLL {
                scrolling.offset.y = scrolling.offset.y.min(0.0).max(-scrollable_height).round();
            }
        }

        if phase == ScrollEventPhase::Start || phase == ScrollEventPhase::Move(true) {
            scrolling.started_bouncing_back = false
        } else if overscrolling &&
            ((delta.x < 1.0 && delta.y < 1.0) || phase == ScrollEventPhase::End)
        {
            scrolling.started_bouncing_back = true;
            scrolling.bouncing_back = true
        }

        if CAN_OVERSCROLL {
            scrolling.stretch_overscroll_spring(overscroll_amount);
        }

        scrolling.offset != original_layer_scroll_offset || scrolling.started_bouncing_back
    }

    pub fn tick_scrolling_bounce_animation(&mut self) {
        if let NodeType::ScrollFrame(ref mut scrolling) = self.node_type {
            scrolling.tick_scrolling_bounce_animation();
        }
    }

    pub fn ray_intersects_node(&self, cursor: &WorldPoint) -> bool {
        let inv = match self.world_viewport_transform.inverse() {
            Some(inv) => inv,
            None => return false,
        };

        let z0 = -10000.0;
        let z1 = 10000.0;

        let p0 = inv.transform_point3d(&cursor.extend(z0));
        let p1 = inv.transform_point3d(&cursor.extend(z1));

        if self.scrollable_size() == LayerSize::zero() {
            return false;
        }

        ray_intersects_rect(
            p0.to_untyped(),
            p1.to_untyped(),
            self.local_viewport_rect.to_untyped(),
        )
    }

    pub fn scroll_offset(&self) -> LayerVector2D {
        match self.node_type {
            NodeType::ScrollFrame(ref scrolling) => scrolling.offset,
            _ => LayerVector2D::zero(),
        }
    }

    pub fn is_overscrolling(&self) -> bool {
        match self.node_type {
            NodeType::ScrollFrame(ref state) => state.overscroll_amount() != LayerVector2D::zero(),
            _ => false,
        }
    }
}

#[derive(Copy, Clone, Debug)]
pub struct ScrollingState {
    pub offset: LayerVector2D,
    pub spring: Spring,
    pub started_bouncing_back: bool,
    pub bouncing_back: bool,
    pub should_handoff_scroll: bool,
    pub scroll_sensitivity: ScrollSensitivity,

    /// Amount that this ScrollFrame can scroll in both directions.
    pub scrollable_size: LayerSize,

}

/// Manages scrolling offset, overscroll state, etc.
impl ScrollingState {
    pub fn new(scroll_sensitivity: ScrollSensitivity,
               scrollable_size: LayerSize
    ) -> ScrollingState {
        ScrollingState {
            offset: LayerVector2D::zero(),
            spring: Spring::at(LayerPoint::zero(), STIFFNESS, DAMPING),
            started_bouncing_back: false,
            bouncing_back: false,
            should_handoff_scroll: false,
            scroll_sensitivity,
            scrollable_size,
        }
    }

    pub fn sensitive_to_input_events(&self) -> bool {
        match self.scroll_sensitivity {
            ScrollSensitivity::ScriptAndInputEvents => true,
            ScrollSensitivity::Script => false,
        }
    }

    pub fn stretch_overscroll_spring(&mut self, overscroll_amount: LayerVector2D) {
        let offset = self.offset.to_point();
        self.spring
            .coords(offset, offset, offset + overscroll_amount);
    }

    pub fn tick_scrolling_bounce_animation(&mut self) {
        let finished = self.spring.animate();
        self.offset = self.spring.current().to_vector();
        if finished {
            self.bouncing_back = false
        }
    }

    pub fn overscroll_amount(&self) -> LayerVector2D {
        let overscroll_x = if self.offset.x > 0.0 {
            -self.offset.x
        } else if self.offset.x < -self.scrollable_size.width {
            -self.scrollable_size.width - self.offset.x
        } else {
            0.0
        };

        let overscroll_y = if self.offset.y > 0.0 {
            -self.offset.y
        } else if self.offset.y < -self.scrollable_size.height {
            -self.scrollable_size.height - self.offset.y
        } else {
            0.0
        };

        LayerVector2D::new(overscroll_x, overscroll_y)
    }
}

/// Contains information about reference frames.
#[derive(Copy, Clone, Debug)]
pub struct ReferenceFrameInfo {
    /// The transformation that establishes this reference frame, relative to the parent
    /// reference frame. The origin of the reference frame is included in the transformation.
    pub transform: LayerToScrollTransform,

    /// The original, not including the transform and relative to the parent reference frame,
    /// origin of this reference frame. This is already rolled into the `transform' property, but
    /// we also store it here to properly transform the viewport for sticky positioning.
    pub origin_in_parent_reference_frame: LayerVector2D,
}
