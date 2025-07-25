// Copyright 2020 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! rutabaga_core: Cross-platform, Rust-based, Wayland and Vulkan centric GPU virtualization.
use std::collections::BTreeMap as Map;
use std::convert::TryInto;
use std::io::IoSlice;
use std::io::IoSliceMut;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

use crate::cross_domain::CrossDomain;
#[cfg(feature = "gfxstream")]
use crate::gfxstream::Gfxstream;
use crate::rutabaga_2d::Rutabaga2D;
use crate::rutabaga_os::MemoryMapping;
use crate::rutabaga_os::OwnedDescriptor;
use crate::rutabaga_utils::*;
use crate::snapshot::RutabagaSnapshotReader;
use crate::snapshot::RutabagaSnapshotWriter;
#[cfg(feature = "virgl_renderer")]
use crate::virgl_renderer::VirglRenderer;

const RUTABAGA_DEFAULT_WIDTH: u32 = 1280;
const RUTABAGA_DEFAULT_HEIGHT: u32 = 1024;

/// Information required for 2D functionality.
#[derive(Clone, Deserialize, Serialize)]
pub struct Rutabaga2DInfo {
    pub width: u32,
    pub height: u32,
    pub host_mem: Vec<u8>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Rutabaga2DSnapshot {
    width: u32,
    height: u32,
    // NOTE: `host_mem` is not preserved to avoid snapshot bloat.
}

/// A Rutabaga resource, supporting 2D and 3D rutabaga features.  Assumes a single-threaded library.
pub struct RutabagaResource {
    pub resource_id: u32,
    pub handle: Option<Arc<RutabagaHandle>>,
    pub blob: bool,
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub map_info: Option<u32>,
    pub info_2d: Option<Rutabaga2DInfo>,
    pub info_3d: Option<Resource3DInfo>,
    pub vulkan_info: Option<VulkanInfo>,
    pub backing_iovecs: Option<Vec<RutabagaIovec>>,
    /// Bitmask of components that have already imported this resource
    pub component_mask: u8,
    pub size: u64,
    pub mapping: Option<MemoryMapping>,
}

/// The preserved fields of `RutabagaResource` that are saved and loaded across snapshot and
/// restore.
#[derive(Deserialize, Serialize)]
struct RutabagaResourceSnapshot {
    resource_id: u32,
    // NOTE: `RutabagaResource::handle` is not included here because OS handles will
    // not be valid across snapshot and restore.  The caller of `Rutagaba::restore()`
    // is expected to re-map resources (via `Rutabaga::map()` or `Rutabaga::export_blob()`)
    // when restoring snapshots.
    blob: bool,
    blob_mem: u32,
    blob_flags: u32,
    map_info: Option<u32>,
    info_2d: Option<Rutabaga2DSnapshot>,
    info_3d: Option<Resource3DInfo>,
    vulkan_info: Option<VulkanInfo>,
    // NOTE: `RutabagaResource::backing_iovecs` isn't snapshotted because the
    // pointers won't be valid at restore time, see the `Rutabaga::restore` doc.
    // If the client doesn't attach new iovecs, the restored resource will
    // behave as if they had been detached (instead of segfaulting on the stale
    // iovec pointers).
    component_mask: u8,
    size: u64,
    // NOTE: `RutabagaResource::mapping` is not included here because mapped resources
    // generally will not be mapped to the same host virtual address across snapshot
    // and restore. The caller of `Rutagaba::restore()` is expected to re-map resources
    // (via `Rutabaga::map()`) when restoring snapshots.
}

impl TryFrom<&RutabagaResource> for RutabagaResourceSnapshot {
    type Error = RutabagaError;
    fn try_from(resource: &RutabagaResource) -> Result<Self, Self::Error> {
        Ok(RutabagaResourceSnapshot {
            resource_id: resource.resource_id,
            blob: resource.blob,
            blob_mem: resource.blob_mem,
            blob_flags: resource.blob_flags,
            map_info: resource.map_info,
            info_2d: resource.info_2d.as_ref().map(|info| Rutabaga2DSnapshot {
                width: info.width,
                height: info.height,
            }),
            info_3d: resource.info_3d,
            vulkan_info: resource.vulkan_info,
            size: resource.size,
            component_mask: resource.component_mask,
        })
    }
}

impl TryFrom<RutabagaResourceSnapshot> for RutabagaResource {
    type Error = RutabagaError;
    fn try_from(snapshot: RutabagaResourceSnapshot) -> Result<Self, Self::Error> {
        Ok(RutabagaResource {
            resource_id: snapshot.resource_id,
            handle: None,
            blob: snapshot.blob,
            blob_mem: snapshot.blob_mem,
            blob_flags: snapshot.blob_flags,
            map_info: snapshot.map_info,
            info_2d: snapshot.info_2d.map(|info| {
                let size = u64::from(info.width * info.height * 4);
                Rutabaga2DInfo {
                    width: info.width,
                    height: info.height,
                    host_mem: vec![0; usize::try_from(size).unwrap()],
                }
            }),
            info_3d: snapshot.info_3d,
            vulkan_info: snapshot.vulkan_info,
            backing_iovecs: None,
            size: snapshot.size,
            component_mask: snapshot.component_mask,
            mapping: None,
        })
    }
}

/// A RutabagaComponent is a building block of the Virtual Graphics Interface (VGI).  Each component
/// on it's own is sufficient to virtualize graphics on many Google products.  These components wrap
/// libraries like gfxstream or virglrenderer, and Rutabaga's own 2D and cross-domain prototype
/// functionality.
///
/// Most methods return a `RutabagaResult` that indicate the success, failure, or requested data for
/// the given command.
pub trait RutabagaComponent {
    /// Implementations should return the version and size of the given capset_id.  (0, 0) is
    /// returned by default.
    fn get_capset_info(&self, _capset_id: u32) -> (u32, u32) {
        (0, 0)
    }

    /// Implementations should return the capabilites of given a `capset_id` and `version`.  A
    /// zero-sized array is returned by default.
    fn get_capset(&self, _capset_id: u32, _version: u32) -> Vec<u8> {
        Vec::new()
    }

    /// Implementations should set their internal context to be the reserved context 0.
    fn force_ctx_0(&self) {}

    /// Implementations must create a fence that represents the completion of prior work.  This is
    /// required for synchronization with the guest kernel.
    fn create_fence(&mut self, _fence: RutabagaFence) -> RutabagaResult<()> {
        Ok(())
    }

    /// Used only by VirglRenderer to poll when its poll_descriptor is signaled.
    fn event_poll(&self) {}

    /// Used only by VirglRenderer to return a poll_descriptor that is signaled when a poll() is
    /// necessary.
    fn poll_descriptor(&self) -> Option<OwnedDescriptor> {
        None
    }

    /// Implementations must create a resource with the given metadata.  For 2D rutabaga components,
    /// this a system memory allocation.  For 3D components, this is typically a GL texture or
    /// buffer.  Vulkan components should use blob resources instead.
    fn create_3d(
        &self,
        resource_id: u32,
        _resource_create_3d: ResourceCreate3D,
    ) -> RutabagaResult<RutabagaResource> {
        Ok(RutabagaResource {
            resource_id,
            handle: None,
            blob: false,
            blob_mem: 0,
            blob_flags: 0,
            map_info: None,
            info_2d: None,
            info_3d: None,
            vulkan_info: None,
            backing_iovecs: None,
            component_mask: 0,
            size: 0,
            mapping: None,
        })
    }

    fn import(
        &self,
        _resource_id: u32,
        _import_handle: RutabagaHandle,
        _import_data: RutabagaImportData,
    ) -> RutabagaResult<Option<RutabagaResource>> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must attach `vecs` to the resource.
    fn attach_backing(
        &self,
        _resource_id: u32,
        _vecs: &mut Vec<RutabagaIovec>,
    ) -> RutabagaResult<()> {
        Ok(())
    }

    /// Implementations must detach `vecs` from the resource.
    fn detach_backing(&self, _resource_id: u32) {}

    /// Implementations must release the guest kernel reference on the resource.
    fn unref_resource(&self, _resource_id: u32) {}

    /// Implementations must perform the transfer write operation.  For 2D rutabaga components, this
    /// done via memcpy().  For 3D components, this is typically done via glTexSubImage(..).
    fn transfer_write(
        &self,
        _ctx_id: u32,
        _resource: &mut RutabagaResource,
        _transfer: Transfer3D,
        _buf: Option<IoSlice>,
    ) -> RutabagaResult<()> {
        Ok(())
    }

    /// Implementations must perform the transfer read operation.  For 2D rutabaga components, this
    /// done via memcpy().  For 3D components, this is typically done via glReadPixels(..).
    fn transfer_read(
        &self,
        _ctx_id: u32,
        _resource: &mut RutabagaResource,
        _transfer: Transfer3D,
        _buf: Option<IoSliceMut>,
    ) -> RutabagaResult<()> {
        Ok(())
    }

    /// Implementations must flush the given resource to the display.
    fn resource_flush(&self, _resource_id: &mut RutabagaResource) -> RutabagaResult<()> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must create a blob resource on success.  The memory parameters, size, and
    /// usage of the blob resource is given by `resource_create_blob`.
    fn create_blob(
        &mut self,
        _ctx_id: u32,
        _resource_id: u32,
        _resource_create_blob: ResourceCreateBlob,
        _iovec_opt: Option<Vec<RutabagaIovec>>,
        _handle_opt: Option<RutabagaHandle>,
    ) -> RutabagaResult<RutabagaResource> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must map the blob resource on success.  This is typically done by
    /// glMapBufferRange(...) or vkMapMemory.
    fn map(&self, _resource_id: u32) -> RutabagaResult<RutabagaMapping> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must unmap the blob resource on success.  This is typically done by
    /// glUnmapBuffer(...) or vkUnmapMemory.
    fn unmap(&self, _resource_id: u32) -> RutabagaResult<()> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must return a RutabagaHandle of the fence on success.
    fn export_fence(&self, _fence_id: u64) -> RutabagaResult<RutabagaHandle> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must create a context for submitting commands.  The command stream of the
    /// context is determined by `context_init`.  For virgl contexts, it is a Gallium/TGSI command
    /// stream.  For gfxstream contexts, it's an autogenerated Vulkan or GLES streams.
    fn create_context(
        &self,
        _ctx_id: u32,
        _context_init: u32,
        _context_name: Option<&str>,
        _fence_handler: RutabagaFenceHandler,
    ) -> RutabagaResult<Box<dyn RutabagaContext>> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations should stop workers.
    fn suspend(&self) -> RutabagaResult<()> {
        Ok(())
    }

    /// Implementations must snapshot to the specified writer.
    fn snapshot(&self, _writer: RutabagaSnapshotWriter) -> RutabagaResult<()> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must restore from the specified reader.
    fn restore(&self, _reader: RutabagaSnapshotReader) -> RutabagaResult<()> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must restore the context from the given stream.
    fn restore_context(
        &self,
        _snapshot: Vec<u8>,
        _fence_handler: RutabagaFenceHandler,
    ) -> RutabagaResult<Box<dyn RutabagaContext>> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations should resume workers.
    fn resume(&self) -> RutabagaResult<()> {
        Ok(())
    }
}

pub trait RutabagaContext {
    /// Implementations must return a RutabagaResource given the `resource_create_blob` parameters.
    fn context_create_blob(
        &mut self,
        _resource_id: u32,
        _resource_create_blob: ResourceCreateBlob,
        _handle_opt: Option<RutabagaHandle>,
    ) -> RutabagaResult<RutabagaResource> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must handle the context-specific command stream.
    fn submit_cmd(
        &mut self,
        _commands: &mut [u8],
        _fence_ids: &[u64],
        shareable_fences: Vec<RutabagaHandle>,
    ) -> RutabagaResult<()>;

    /// Implementations may use `resource` in this context's command stream.
    fn attach(&mut self, _resource: &mut RutabagaResource);

    /// Implementations must stop using `resource` in this context's command stream.
    fn detach(&mut self, _resource: &RutabagaResource);

    /// Implementations must create a fence on specified `ring_idx` in `fence`.  This
    /// allows for multiple synchronizations timelines per RutabagaContext.
    ///
    /// If RUTABAGA_FLAG_FENCE_HOST_SHAREABLE is set, a rutabaga handle must be returned on
    /// success.
    fn context_create_fence(
        &mut self,
        _fence: RutabagaFence,
    ) -> RutabagaResult<Option<RutabagaHandle>> {
        Err(RutabagaErrorKind::Unsupported.into())
    }

    /// Implementations must return the component type associated with the context.
    fn component_type(&self) -> RutabagaComponentType;

    /// Implementations must serialize the context.
    fn snapshot(&self) -> RutabagaResult<Vec<u8>> {
        Err(RutabagaErrorKind::Unsupported.into())
    }
}

#[derive(Copy, Clone)]
struct RutabagaCapsetInfo {
    pub capset_id: u32,
    pub component: RutabagaComponentType,
    pub name: &'static str,
}

const RUTABAGA_CAPSETS: [RutabagaCapsetInfo; 9] = [
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_VIRGL,
        component: RutabagaComponentType::VirglRenderer,
        name: "virgl",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_VIRGL2,
        component: RutabagaComponentType::VirglRenderer,
        name: "virgl2",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_GFXSTREAM_VULKAN,
        component: RutabagaComponentType::Gfxstream,
        name: "gfxstream-vulkan",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_VENUS,
        component: RutabagaComponentType::VirglRenderer,
        name: "venus",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_CROSS_DOMAIN,
        component: RutabagaComponentType::CrossDomain,
        name: "cross-domain",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_DRM,
        component: RutabagaComponentType::VirglRenderer,
        name: "drm",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_GFXSTREAM_MAGMA,
        component: RutabagaComponentType::Gfxstream,
        name: "gfxstream-magma",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_GFXSTREAM_GLES,
        component: RutabagaComponentType::Gfxstream,
        name: "gfxstream-gles",
    },
    RutabagaCapsetInfo {
        capset_id: RUTABAGA_CAPSET_GFXSTREAM_COMPOSER,
        component: RutabagaComponentType::Gfxstream,
        name: "gfxstream-composer",
    },
];

pub fn calculate_capset_mask<'a, I: Iterator<Item = &'a str>>(context_names: I) -> u64 {
    let mut capset_mask = 0;
    for name in context_names {
        if let Some(capset) = RUTABAGA_CAPSETS.iter().find(|capset| capset.name == name) {
            capset_mask |= 1 << capset.capset_id;
        };
    }

    capset_mask
}

pub fn calculate_capset_names(capset_mask: u64) -> Vec<String> {
    RUTABAGA_CAPSETS
        .iter()
        .filter(|capset| capset_mask & (1 << capset.capset_id) != 0)
        .map(|capset| capset.name.to_string())
        .collect()
}

fn calculate_component(component_mask: u8) -> RutabagaResult<RutabagaComponentType> {
    if component_mask.count_ones() != 1 {
        return Err(anyhow::anyhow!("can't infer single component")
            .context(RutabagaErrorKind::SpecViolation)
            .into());
    }

    match component_mask.trailing_zeros() {
        0 => Ok(RutabagaComponentType::Rutabaga2D),
        1 => Ok(RutabagaComponentType::VirglRenderer),
        2 => Ok(RutabagaComponentType::Gfxstream),
        3 => Ok(RutabagaComponentType::CrossDomain),
        _ => Err(RutabagaErrorKind::InvalidComponent.into()),
    }
}

/// The global libary handle used to query capability sets, create resources and contexts.
///
/// Currently, Rutabaga only supports one default component.  Many components running at the
/// same time is a stretch goal of Rutabaga GFX.
///
/// Not thread-safe, but can be made so easily.  Making non-Rutabaga, C/C++ components
/// thread-safe is more difficult.
pub struct Rutabaga {
    resources: Map<u32, RutabagaResource>,
    #[cfg(fence_passing_option1)]
    shareable_fences: Map<u64, RutabagaHandle>,
    contexts: Map<u32, Box<dyn RutabagaContext>>,
    // Declare components after resources and contexts such that it is dropped last.
    components: Map<RutabagaComponentType, Box<dyn RutabagaComponent>>,
    default_component: RutabagaComponentType,
    capset_info: Vec<RutabagaCapsetInfo>,
    fence_handler: RutabagaFenceHandler,
}

/// The serialized and deserialized parts of `Rutabaga` that are preserved across
/// snapshot() and restore().
#[derive(Deserialize, Serialize)]
struct RutabagaSnapshot {
    resources: Map<u32, RutabagaResourceSnapshot>,
    contexts: Map<u32, Vec<u8>>,
}

impl Rutabaga {
    pub fn suspend(&self) -> RutabagaResult<()> {
        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        component.suspend()
    }

    /// Take a snapshot of Rutabaga's current state. The snapshot is serialized into an opaque byte
    /// stream and written to `w`.
    pub fn snapshot(&self, directory: &Path) -> RutabagaResult<()> {
        let snapshot_writer = RutabagaSnapshotWriter::from_existing(directory);

        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let component_snapshot_writer =
            snapshot_writer.add_namespace(self.default_component.as_str())?;
        component.snapshot(component_snapshot_writer)?;

        let snapshot = RutabagaSnapshot {
            resources: self
                .resources
                .iter()
                .map(|(i, r)| Ok((*i, RutabagaResourceSnapshot::try_from(r)?)))
                .collect::<RutabagaResult<_>>()?,
            contexts: self
                .contexts
                .iter()
                .map(|(i, c)| Ok((*i, c.snapshot()?)))
                .collect::<RutabagaResult<_>>()?,
        };
        snapshot_writer.add_fragment("rutabaga_snapshot", &snapshot)
    }

    fn destroy_objects(&mut self) -> RutabagaResult<()> {
        let resource_ids: Vec<_> = self.resources.keys().cloned().collect();
        resource_ids
            .into_iter()
            .try_for_each(|resource_id| self.unref_resource(resource_id))?;

        self.contexts.clear();

        Ok(())
    }

    /// Restore Rutabaga to a previously snapshot'd state.
    ///
    /// Snapshotting on one host machine and then restoring on another ("host migration") might
    /// work for very similar machines but isn't explicitly supported yet.
    ///
    /// Rutabaga will recreate resources internally, but it's the VMM's responsibility to re-attach
    /// backing iovecs and re-map the memory after re-creation. Specifically:
    ///
    /// * Mode2D
    ///    * The VMM must call `Rutabaga::attach_backing` calls for all resources that had backing
    ///      memory at the time of the snapshot.
    /// * ModeVirglRenderer
    ///    * Not supported.
    /// * ModeGfxstream
    ///    * WiP support.
    ///
    /// NOTES: This is required because the pointers to backing memory aren't stable, help from the
    /// VMM is necessary. In an alternative approach, the VMM could supply Rutabaga with callbacks
    /// to translate to/from stable guest physical addresses, but it is unclear how well that
    /// approach would scale to support 3D modes, which have others problems that require VMM help,
    /// like resource handles.
    pub fn restore(&mut self, directory: &Path) -> RutabagaResult<()> {
        self.destroy_objects()?;

        let snapshot_reader = RutabagaSnapshotReader::from_existing(directory)?;

        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let component_snapshot_reader =
            snapshot_reader.get_namespace(self.default_component.as_str())?;
        component.restore(component_snapshot_reader)?;

        let snapshot: RutabagaSnapshot = snapshot_reader.get_fragment("rutabaga_snapshot")?;

        self.resources = snapshot
            .resources
            .into_iter()
            .map(|(i, s)| Ok((i, RutabagaResource::try_from(s)?)))
            .collect::<RutabagaResult<_>>()?;
        self.contexts = snapshot
            .contexts
            .into_iter()
            .map(|(i, c)| Ok((i, component.restore_context(c, self.fence_handler.clone())?)))
            .collect::<RutabagaResult<_>>()?;

        Ok(())
    }

    pub fn resume(&self) -> RutabagaResult<()> {
        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        component.resume()
    }

    fn capset_id_to_component_type(&self, capset_id: u32) -> RutabagaResult<RutabagaComponentType> {
        let component = self
            .capset_info
            .iter()
            .find(|capset_info| capset_info.capset_id == capset_id)
            .ok_or(RutabagaErrorKind::InvalidCapset)?
            .component;

        Ok(component)
    }

    fn capset_index_to_component_info(&self, index: u32) -> RutabagaResult<RutabagaCapsetInfo> {
        let idx = index as usize;
        if idx >= self.capset_info.len() {
            return Err(RutabagaErrorKind::InvalidCapset.into());
        }

        Ok(self.capset_info[idx])
    }

    /// Gets the version and size for the capabilty set `index`.
    pub fn get_capset_info(&self, index: u32) -> RutabagaResult<(u32, u32, u32)> {
        let capset_info = self.capset_index_to_component_info(index)?;

        let component = self
            .components
            .get(&capset_info.component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let (capset_version, capset_size) = component.get_capset_info(capset_info.capset_id);
        Ok((capset_info.capset_id, capset_version, capset_size))
    }

    /// Gets the capability set for the `capset_id` and `version`.
    /// Each capability set is associated with a context type, which is associated
    /// with a rutabaga component.
    pub fn get_capset(&self, capset_id: u32, version: u32) -> RutabagaResult<Vec<u8>> {
        // The default workaround is just until context types are fully supported in all
        // Google kernels.
        let component_type = self
            .capset_id_to_component_type(capset_id)
            .unwrap_or(self.default_component);

        let component = self
            .components
            .get(&component_type)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        Ok(component.get_capset(capset_id, version))
    }

    /// Gets the number of capsets
    pub fn get_num_capsets(&self) -> u32 {
        self.capset_info.len() as u32
    }

    /// Forces context zero for the default rutabaga component.
    pub fn force_ctx_0(&self) {
        if let Some(component) = self.components.get(&self.default_component) {
            component.force_ctx_0();
        }
    }

    /// Creates a fence with the given `fence`.
    /// If the flags include RUTABAGA_FLAG_INFO_RING_IDX, then the fence is created on a
    /// specific timeline on the specific context.
    pub fn create_fence(&mut self, fence: RutabagaFence) -> RutabagaResult<()> {
        if fence.flags & RUTABAGA_FLAG_INFO_RING_IDX != 0 {
            let ctx = self
                .contexts
                .get_mut(&fence.ctx_id)
                .ok_or(RutabagaErrorKind::InvalidContextId)?;

            #[allow(unused_variables)]
            let handle_opt = ctx.context_create_fence(fence)?;

            #[cfg(fence_passing_option1)]
            if fence.flags & RUTABAGA_FLAG_FENCE_HOST_SHAREABLE != 0 {
                let handle = handle_opt.unwrap();
                self.shareable_fences.insert(fence.fence_id, handle);
            }
        } else {
            let component = self
                .components
                .get_mut(&self.default_component)
                .ok_or(RutabagaErrorKind::InvalidComponent)?;

            component.create_fence(fence)?;
        }

        Ok(())
    }

    /// Polls the default rutabaga component.
    pub fn event_poll(&self) {
        if let Some(component) = self.components.get(&self.default_component) {
            component.event_poll();
        }
    }

    /// Returns a pollable descriptor for the default rutabaga component. In practice, it is only
    /// not None if the default component is virglrenderer.
    pub fn poll_descriptor(&self) -> Option<OwnedDescriptor> {
        let component = self.components.get(&self.default_component).or(None)?;
        component.poll_descriptor()
    }

    /// Creates a resource with the `resource_create_3d` metadata.
    pub fn resource_create_3d(
        &mut self,
        resource_id: u32,
        resource_create_3d: ResourceCreate3D,
    ) -> RutabagaResult<()> {
        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        if self.resources.contains_key(&resource_id) {
            return Err(RutabagaErrorKind::InvalidResourceId.into());
        }

        let resource = component.create_3d(resource_id, resource_create_3d)?;
        self.resources.insert(resource_id, resource);
        Ok(())
    }

    /// Creates and imports to a resource with the external `import_handle` and the `import_data`
    /// metadata.
    pub fn resource_import(
        &mut self,
        resource_id: u32,
        import_handle: RutabagaHandle,
        import_data: RutabagaImportData,
    ) -> RutabagaResult<()> {
        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        match component.import(resource_id, import_handle, import_data) {
            Ok(Some(resource)) => {
                self.resources.insert(resource_id, resource);
            }
            Ok(None) => {
                if !self.resources.contains_key(&resource_id) {
                    return Err(RutabagaErrorKind::InvalidResourceId.into());
                }
            }
            Err(e) => return Err(e),
        };
        Ok(())
    }

    /// Attaches `vecs` to the resource.
    pub fn attach_backing(
        &mut self,
        resource_id: u32,
        mut vecs: Vec<RutabagaIovec>,
    ) -> RutabagaResult<()> {
        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.attach_backing(resource_id, &mut vecs)?;
        resource.backing_iovecs = Some(vecs);
        Ok(())
    }

    /// Detaches any previously attached iovecs from the resource.
    pub fn detach_backing(&mut self, resource_id: u32) -> RutabagaResult<()> {
        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.detach_backing(resource_id);
        resource.backing_iovecs = None;
        Ok(())
    }

    /// Releases guest kernel reference on the resource.
    pub fn unref_resource(&mut self, resource_id: u32) -> RutabagaResult<()> {
        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        self.resources
            .remove(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.unref_resource(resource_id);
        Ok(())
    }

    /// For HOST3D_GUEST resources, copies from the attached iovecs to the host resource.  For
    /// HOST3D resources, this may flush caches, though this feature is unused by guest userspace.
    pub fn transfer_write(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        transfer: Transfer3D,
        buf: Option<IoSlice>,
    ) -> RutabagaResult<()> {
        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.transfer_write(ctx_id, resource, transfer, buf)
    }

    /// 1) If specified, copies to `buf` from the host resource.
    /// 2) Otherwise, for HOST3D_GUEST resources, copies to the attached iovecs from the host
    ///    resource.  For HOST3D resources, this may invalidate caches, though this feature is
    ///    unused by guest userspace.
    pub fn transfer_read(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        transfer: Transfer3D,
        buf: Option<IoSliceMut>,
    ) -> RutabagaResult<()> {
        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.transfer_read(ctx_id, resource, transfer, buf)
    }

    pub fn resource_flush(&mut self, resource_id: u32) -> RutabagaResult<()> {
        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::Unsupported)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        component.resource_flush(resource)
    }

    /// Creates a blob resource with the `ctx_id` and `resource_create_blob` metadata.
    /// Associates `iovecs` with the resource, if there are any.  Associates externally
    /// created `handle` with the resource, if there is any.
    pub fn resource_create_blob(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        resource_create_blob: ResourceCreateBlob,
        iovecs: Option<Vec<RutabagaIovec>>,
        handle: Option<RutabagaHandle>,
    ) -> RutabagaResult<()> {
        if self.resources.contains_key(&resource_id) {
            return Err(RutabagaErrorKind::InvalidResourceId.into());
        }

        let component = self
            .components
            .get_mut(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        let mut context = None;
        // For the cross-domain context, we'll need to create the blob resource via a home-grown
        // rutabaga context rather than one from an external C/C++ component.  Use `ctx_id` and
        // the component type if it happens to be a cross-domain context.
        if ctx_id > 0 {
            let ctx = self
                .contexts
                .get_mut(&ctx_id)
                .ok_or(RutabagaErrorKind::InvalidContextId)?;

            if ctx.component_type() == RutabagaComponentType::CrossDomain {
                context = Some(ctx);
            }
        }

        let resource = match context {
            Some(ctx) => ctx.context_create_blob(resource_id, resource_create_blob, handle)?,
            None => {
                component.create_blob(ctx_id, resource_id, resource_create_blob, iovecs, handle)?
            }
        };

        self.resources.insert(resource_id, resource);
        Ok(())
    }

    /// Returns a memory mapping of the blob resource.
    pub fn map(&mut self, resource_id: u32) -> RutabagaResult<RutabagaMapping> {
        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        let component_type = calculate_component(resource.component_mask)?;
        if component_type == RutabagaComponentType::CrossDomain {
            let handle_opt = resource.handle.take();
            match handle_opt {
                Some(handle) => {
                    if handle.handle_type != RUTABAGA_HANDLE_TYPE_MEM_SHM {
                        return Err(anyhow::anyhow!("expected a shared memory handle")
                            .context(RutabagaErrorKind::SpecViolation)
                            .into());
                    }

                    let clone = handle.try_clone()?;
                    let resource_size: usize = resource.size.try_into()?;
                    let map_info = resource
                        .map_info
                        .context("no map info available")
                        .context(RutabagaErrorKind::SpecViolation)?;

                    // Creating the mapping closes the cloned descriptor.
                    let mapping = MemoryMapping::from_safe_descriptor(
                        clone.os_handle,
                        resource_size,
                        map_info,
                    )?;
                    let rutabaga_mapping = mapping.as_rutabaga_mapping();
                    resource.handle = Some(handle);
                    resource.mapping = Some(mapping);

                    return Ok(rutabaga_mapping);
                }
                None => {
                    return Err(anyhow::anyhow!("expected a handle to map")
                        .context(RutabagaErrorKind::SpecViolation)
                        .into())
                }
            }
        }

        let component = self
            .components
            .get(&component_type)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        component.map(resource_id)
    }

    /// Unmaps the blob resource from the default component
    pub fn unmap(&mut self, resource_id: u32) -> RutabagaResult<()> {
        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        let component_type = calculate_component(resource.component_mask)?;
        if component_type == RutabagaComponentType::CrossDomain {
            resource.mapping = None;
            return Ok(());
        }

        let component = self
            .components
            .get(&component_type)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        component.unmap(resource_id)
    }

    /// Returns the `map_info` of the blob resource. The valid values for `map_info`
    /// are defined in the virtio-gpu spec.
    pub fn map_info(&self, resource_id: u32) -> RutabagaResult<u32> {
        let resource = self
            .resources
            .get(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        resource
            .map_info
            .context("no map info available")
            .context(RutabagaErrorKind::SpecViolation)
            .map_err(|e| e.into())
    }

    /// Returns the `vulkan_info` of the blob resource, which consists of the physical device
    /// index and memory index associated with the resource.
    pub fn vulkan_info(&self, resource_id: u32) -> RutabagaResult<VulkanInfo> {
        let resource = self
            .resources
            .get(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        resource
            .vulkan_info
            .ok_or(RutabagaErrorKind::InvalidVulkanInfo.into())
    }

    /// Returns the 3D info associated with the resource, if any.
    pub fn query(&self, resource_id: u32) -> RutabagaResult<Resource3DInfo> {
        let resource = self
            .resources
            .get(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        resource
            .info_3d
            .context("no 3d info available")
            .context(RutabagaErrorKind::SpecViolation)
            .map_err(|e| e.into())
    }

    /// Exports a blob resource.  See virtio-gpu spec for blob flag use flags.
    pub fn export_blob(&mut self, resource_id: u32) -> RutabagaResult<RutabagaHandle> {
        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        // We can inspect blob flags only once guest minigbm is fully transitioned to blob.
        let share_mask = RUTABAGA_BLOB_FLAG_USE_SHAREABLE | RUTABAGA_BLOB_FLAG_USE_CROSS_DEVICE;
        let shareable = (resource.blob_flags & share_mask != 0) || !resource.blob;

        let opt = resource.handle.take();

        match (opt, shareable) {
            (Some(handle), true) => {
                let clone = handle.try_clone()?;
                resource.handle = Some(handle);
                Ok(clone)
            }
            (Some(handle), false) => {
                // Exactly one strong reference in this case.
                let hnd = Arc::try_unwrap(handle)
                    .map_err(|_| RutabagaErrorKind::InvalidRutabagaHandle)?;
                Ok(hnd)
            }
            _ => Err(RutabagaErrorKind::InvalidRutabagaHandle.into()),
        }
    }

    /// Exports the given fence for import into other processes.
    pub fn export_fence(&mut self, fence_id: u64) -> RutabagaResult<RutabagaHandle> {
        #[cfg(fence_passing_option1)]
        if let Some(handle) = self.shareable_fences.get_mut(&fence_id) {
            return handle.try_clone();
        }

        let component = self
            .components
            .get(&self.default_component)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        component.export_fence(fence_id)
    }

    /// Creates a context with the given `ctx_id` and `context_init` variable.
    /// `context_init` is used to determine which rutabaga component creates the context.
    pub fn create_context(
        &mut self,
        ctx_id: u32,
        context_init: u32,
        context_name: Option<&str>,
    ) -> RutabagaResult<()> {
        // The default workaround is just until context types are fully supported in all
        // Google kernels.
        let capset_id = context_init & RUTABAGA_CONTEXT_INIT_CAPSET_ID_MASK;
        let component_type = self
            .capset_id_to_component_type(capset_id)
            .unwrap_or(self.default_component);

        let component = self
            .components
            .get_mut(&component_type)
            .ok_or(RutabagaErrorKind::InvalidComponent)?;

        if self.contexts.contains_key(&ctx_id) {
            return Err(RutabagaErrorKind::InvalidContextId.into());
        }

        let ctx = component.create_context(
            ctx_id,
            context_init,
            context_name,
            self.fence_handler.clone(),
        )?;
        self.contexts.insert(ctx_id, ctx);
        Ok(())
    }

    /// Destroys the context given by `ctx_id`.
    pub fn destroy_context(&mut self, ctx_id: u32) -> RutabagaResult<()> {
        self.contexts
            .remove(&ctx_id)
            .ok_or(RutabagaErrorKind::InvalidContextId)?;
        Ok(())
    }

    /// Attaches the resource given by `resource_id` to the context given by `ctx_id`.
    pub fn context_attach_resource(&mut self, ctx_id: u32, resource_id: u32) -> RutabagaResult<()> {
        let ctx = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(RutabagaErrorKind::InvalidContextId)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        ctx.attach(resource);
        Ok(())
    }

    /// Detaches the resource given by `resource_id` from the context given by `ctx_id`.
    pub fn context_detach_resource(&mut self, ctx_id: u32, resource_id: u32) -> RutabagaResult<()> {
        let ctx = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(RutabagaErrorKind::InvalidContextId)?;

        let resource = self
            .resources
            .get_mut(&resource_id)
            .ok_or(RutabagaErrorKind::InvalidResourceId)?;

        ctx.detach(resource);
        Ok(())
    }

    /// submits `commands` to the context given by `ctx_id`.
    pub fn submit_command(
        &mut self,
        ctx_id: u32,
        commands: &mut [u8],
        fence_ids: &[u64],
    ) -> RutabagaResult<()> {
        let ctx = self
            .contexts
            .get_mut(&ctx_id)
            .ok_or(RutabagaErrorKind::InvalidContextId)?;

        #[allow(unused_mut)]
        let mut shareable_fences: Vec<RutabagaHandle> = Vec::with_capacity(fence_ids.len());

        #[cfg(fence_passing_option1)]
        for (i, fence_id) in fence_ids.iter().enumerate() {
            let handle = self
                .shareable_fences
                .get_mut(fence_id)
                .ok_or(RutabagaErrorKind::InvalidRutabagaHandle)?;

            let clone = handle.try_clone()?;
            shareable_fences.insert(i, clone);
        }

        ctx.submit_cmd(commands, fence_ids, shareable_fences)
    }

    /// destroy fences that are still outstanding
    #[cfg(fence_passing_option1)]
    pub fn destroy_fences(&mut self, fence_ids: &[u64]) -> RutabagaResult<()> {
        for fence_id in fence_ids {
            self.shareable_fences
                .remove(fence_id)
                .ok_or(RutabagaErrorKind::InvalidRutabagaHandle)?;
        }

        Ok(())
    }
}

/// Rutabaga Builder, following the Rust builder pattern.
#[derive(Clone)]
pub struct RutabagaBuilder {
    display_width: u32,
    display_height: u32,
    default_component: RutabagaComponentType,
    gfxstream_flags: GfxstreamFlags,
    virglrenderer_flags: VirglRendererFlags,
    capset_mask: u64,
    channels: Option<Vec<RutabagaChannel>>,
    debug_handler: Option<RutabagaDebugHandler>,
    renderer_features: Option<String>,
}

impl RutabagaBuilder {
    /// Create new a RutabagaBuilder.
    pub fn new(default_component: RutabagaComponentType, capset_mask: u64) -> RutabagaBuilder {
        let virglrenderer_flags = VirglRendererFlags::new()
            .use_thread_sync(true)
            .use_async_fence_cb(true);
        let gfxstream_flags = GfxstreamFlags::new();
        RutabagaBuilder {
            display_width: RUTABAGA_DEFAULT_WIDTH,
            display_height: RUTABAGA_DEFAULT_HEIGHT,
            default_component,
            gfxstream_flags,
            virglrenderer_flags,
            capset_mask,
            channels: None,
            debug_handler: None,
            renderer_features: None,
        }
    }

    /// Set display width for the RutabagaBuilder
    pub fn set_display_width(mut self, display_width: u32) -> RutabagaBuilder {
        self.display_width = display_width;
        self
    }

    /// Set display height for the RutabagaBuilder
    pub fn set_display_height(mut self, display_height: u32) -> RutabagaBuilder {
        self.display_height = display_height;
        self
    }

    /// Sets use EGL flags in gfxstream + virglrenderer.
    pub fn set_use_egl(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_egl(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_egl(v);
        self
    }

    /// Sets use GLES in gfxstream + virglrenderer.
    pub fn set_use_gles(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_gles(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_gles(v);
        self
    }

    /// Sets use GLX flags in gfxstream + virglrenderer.
    pub fn set_use_glx(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_glx(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_glx(v);
        self
    }

    /// Sets use surfaceless flags in gfxstream + virglrenderer.
    pub fn set_use_surfaceless(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_surfaceless(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_surfaceless(v);
        self
    }

    /// Sets use Vulkan in gfxstream + virglrenderer.
    pub fn set_use_vulkan(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_vulkan(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_venus(v);
        self
    }

    /// Sets use external blob in gfxstream + virglrenderer.
    pub fn set_use_external_blob(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_external_blob(v);
        self.virglrenderer_flags = self.virglrenderer_flags.use_external_blob(v);
        self
    }

    /// Sets use system blob in gfxstream.
    pub fn set_use_system_blob(mut self, v: bool) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.use_system_blob(v);
        self
    }

    /// Sets use render server in virglrenderer.
    pub fn set_use_render_server(mut self, v: bool) -> RutabagaBuilder {
        self.virglrenderer_flags = self.virglrenderer_flags.use_render_server(v);
        self
    }

    /// Use the Vulkan swapchain to draw on the host window for gfxstream.
    pub fn set_wsi(mut self, v: RutabagaWsi) -> RutabagaBuilder {
        self.gfxstream_flags = self.gfxstream_flags.set_wsi(v);
        self
    }

    /// Set rutabaga channels for the RutabagaBuilder
    pub fn set_rutabaga_channels(
        mut self,
        channels: Option<Vec<RutabagaChannel>>,
    ) -> RutabagaBuilder {
        self.channels = channels;
        self
    }

    /// Set debug handler for the RutabagaBuilder
    pub fn set_debug_handler(
        mut self,
        debug_handler: Option<RutabagaDebugHandler>,
    ) -> RutabagaBuilder {
        self.debug_handler = debug_handler;
        self
    }

    /// Set renderer features for the RutabagaBuilder
    pub fn set_renderer_features(mut self, renderer_features: Option<String>) -> RutabagaBuilder {
        self.renderer_features = renderer_features;
        self
    }

    /// Builds Rutabaga and returns a handle to it.
    ///
    /// This should be only called once per every virtual machine instance.  Rutabaga tries to
    /// intialize all 3D components which have been built. In 2D mode, only the 2D component is
    /// initialized.
    pub fn build(
        mut self,
        fence_handler: RutabagaFenceHandler,
        #[allow(unused_variables)] rutabaga_server_descriptor: Option<OwnedDescriptor>,
    ) -> RutabagaResult<Rutabaga> {
        let mut rutabaga_components: Map<RutabagaComponentType, Box<dyn RutabagaComponent>> =
            Default::default();

        #[allow(unused_mut)]
        let mut rutabaga_capsets: Vec<RutabagaCapsetInfo> = Default::default();

        let capset_enabled =
            |capset_id: u32| -> bool { (self.capset_mask & (1 << capset_id)) != 0 };

        let mut push_capset = |capset_id: u32| {
            if let Some(capset) = RUTABAGA_CAPSETS
                .iter()
                .find(|capset| capset_id == capset.capset_id)
            {
                if self.capset_mask != 0 {
                    if capset_enabled(capset.capset_id) {
                        rutabaga_capsets.push(*capset);
                    }
                } else {
                    // Unconditionally push capset -- this should eventually be deleted when context
                    // types are always specified by crosvm launchers.
                    rutabaga_capsets.push(*capset);
                }
            };
        };

        if self.capset_mask != 0 {
            let supports_gfxstream = capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_VULKAN)
                | capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_MAGMA)
                | capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_GLES)
                | capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_COMPOSER);
            let supports_virglrenderer = capset_enabled(RUTABAGA_CAPSET_VIRGL2)
                | capset_enabled(RUTABAGA_CAPSET_VENUS)
                | capset_enabled(RUTABAGA_CAPSET_DRM);

            if supports_gfxstream {
                self.default_component = RutabagaComponentType::Gfxstream;
            } else if supports_virglrenderer {
                self.default_component = RutabagaComponentType::VirglRenderer;
            } else {
                self.default_component = RutabagaComponentType::CrossDomain;
            }

            self.virglrenderer_flags = self
                .virglrenderer_flags
                .use_virgl(capset_enabled(RUTABAGA_CAPSET_VIRGL2))
                .use_venus(capset_enabled(RUTABAGA_CAPSET_VENUS))
                .use_drm(capset_enabled(RUTABAGA_CAPSET_DRM));

            self.gfxstream_flags = self
                .gfxstream_flags
                .use_gles(capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_GLES))
                .use_vulkan(capset_enabled(RUTABAGA_CAPSET_GFXSTREAM_VULKAN))
        }

        // Make sure that disabled components are not used as default.
        #[cfg(not(feature = "virgl_renderer"))]
        if self.default_component == RutabagaComponentType::VirglRenderer {
            return Err(anyhow::anyhow!("virgl renderer feature not enabled")
                .context(RutabagaErrorKind::InvalidRutabagaBuild)
                .into());
        }
        #[cfg(not(feature = "gfxstream"))]
        if self.default_component == RutabagaComponentType::Gfxstream {
            return Err(anyhow::anyhow!("gfxstream feature not enabled")
                .context(RutabagaErrorKind::InvalidRutabagaBuild)
                .into());
        }

        if self.default_component != RutabagaComponentType::Rutabaga2D {
            #[cfg(feature = "virgl_renderer")]
            if self.default_component == RutabagaComponentType::VirglRenderer {
                if let Ok(virgl) = VirglRenderer::init(
                    self.virglrenderer_flags,
                    fence_handler.clone(),
                    rutabaga_server_descriptor,
                ) {
                    rutabaga_components.insert(RutabagaComponentType::VirglRenderer, virgl);

                    push_capset(RUTABAGA_CAPSET_VIRGL);
                    push_capset(RUTABAGA_CAPSET_VIRGL2);
                    push_capset(RUTABAGA_CAPSET_VENUS);
                    push_capset(RUTABAGA_CAPSET_DRM);
                } else {
                    log::warn!("error initializing gpu backend=virglrenderer, falling back to 2d.");
                    self.default_component = RutabagaComponentType::Rutabaga2D;
                };
            }

            #[cfg(feature = "gfxstream")]
            if self.default_component == RutabagaComponentType::Gfxstream {
                let gfxstream = Gfxstream::init(
                    self.display_width,
                    self.display_height,
                    self.gfxstream_flags,
                    self.renderer_features,
                    fence_handler.clone(),
                    self.debug_handler.clone(),
                )?;

                rutabaga_components.insert(RutabagaComponentType::Gfxstream, gfxstream);

                push_capset(RUTABAGA_CAPSET_GFXSTREAM_VULKAN);
                push_capset(RUTABAGA_CAPSET_GFXSTREAM_MAGMA);
                push_capset(RUTABAGA_CAPSET_GFXSTREAM_GLES);
                push_capset(RUTABAGA_CAPSET_GFXSTREAM_COMPOSER);
            }

            let cross_domain = CrossDomain::init(self.channels, fence_handler.clone())?;
            rutabaga_components.insert(RutabagaComponentType::CrossDomain, cross_domain);
            push_capset(RUTABAGA_CAPSET_CROSS_DOMAIN);
        }

        if self.default_component == RutabagaComponentType::Rutabaga2D {
            let rutabaga_2d = Rutabaga2D::init(fence_handler.clone())?;
            rutabaga_components.insert(RutabagaComponentType::Rutabaga2D, rutabaga_2d);
        }

        Ok(Rutabaga {
            resources: Default::default(),
            #[cfg(fence_passing_option1)]
            shareable_fences: Default::default(),
            contexts: Default::default(),
            components: rutabaga_components,
            default_component: self.default_component,
            capset_info: rutabaga_capsets,
            fence_handler,
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    fn new_2d() -> Rutabaga {
        RutabagaBuilder::new(RutabagaComponentType::Rutabaga2D, 0)
            .build(RutabagaHandler::new(|_| {}), None)
            .unwrap()
    }

    #[test]
    fn snapshot_restore_2d_no_resources() {
        let snapshot_dir = tempfile::tempdir().unwrap();

        let rutabaga1 = new_2d();
        rutabaga1.snapshot(snapshot_dir.path()).unwrap();

        let mut rutabaga1 = new_2d();
        rutabaga1.restore(snapshot_dir.path()).unwrap();
    }

    #[test]
    fn snapshot_restore_2d_one_resource() {
        let snapshot_dir = tempfile::tempdir().unwrap();

        let resource_id = 123;
        let resource_create_3d = ResourceCreate3D {
            target: RUTABAGA_PIPE_TEXTURE_2D,
            format: 1,
            bind: RUTABAGA_PIPE_BIND_RENDER_TARGET,
            width: 100,
            height: 200,
            depth: 1,
            array_size: 1,
            last_level: 0,
            nr_samples: 0,
            flags: 0,
        };

        let mut rutabaga1 = new_2d();
        rutabaga1
            .resource_create_3d(resource_id, resource_create_3d)
            .unwrap();
        rutabaga1
            .attach_backing(
                resource_id,
                vec![RutabagaIovec {
                    base: std::ptr::null_mut(),
                    len: 456,
                }],
            )
            .unwrap();
        rutabaga1.snapshot(snapshot_dir.path()).unwrap();

        let mut rutabaga2 = new_2d();
        rutabaga2.restore(snapshot_dir.path()).unwrap();

        assert_eq!(rutabaga2.resources.len(), 1);
        let rutabaga_resource = rutabaga2.resources.get(&resource_id).unwrap();
        assert_eq!(rutabaga_resource.resource_id, resource_id);
        assert_eq!(
            rutabaga_resource.info_2d.as_ref().unwrap().width,
            resource_create_3d.width
        );
        assert_eq!(
            rutabaga_resource.info_2d.as_ref().unwrap().height,
            resource_create_3d.height
        );
        // NOTE: We attached an backing iovec, but it should be gone post-restore.
        assert!(rutabaga_resource.backing_iovecs.is_none());
    }
}
