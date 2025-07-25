// Copyright 2024 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::btree_map::Entry;
use std::collections::BTreeMap as Map;
use std::collections::BTreeSet as Set;
use std::os::raw::c_void;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use log::error;
use rutabaga_gfx::calculate_capset_mask;
use rutabaga_gfx::kumquat_support::kumquat_gpu_protocol::*;
use rutabaga_gfx::kumquat_support::RutabagaEvent;
use rutabaga_gfx::kumquat_support::RutabagaMemoryMapping;
use rutabaga_gfx::kumquat_support::RutabagaSharedMemory;
use rutabaga_gfx::kumquat_support::RutabagaStream;
use rutabaga_gfx::kumquat_support::RutabagaTube;
use rutabaga_gfx::ResourceCreate3D;
use rutabaga_gfx::ResourceCreateBlob;
use rutabaga_gfx::Rutabaga;
use rutabaga_gfx::RutabagaAsBorrowedDescriptor as AsBorrowedDescriptor;
use rutabaga_gfx::RutabagaBuilder;
use rutabaga_gfx::RutabagaComponentType;
use rutabaga_gfx::RutabagaDescriptor;
use rutabaga_gfx::RutabagaError;
use rutabaga_gfx::RutabagaErrorKind;
use rutabaga_gfx::RutabagaFence;
use rutabaga_gfx::RutabagaFenceHandler;
use rutabaga_gfx::RutabagaHandle;
use rutabaga_gfx::RutabagaIovec;
use rutabaga_gfx::RutabagaResult;
use rutabaga_gfx::RutabagaWsi;
use rutabaga_gfx::Transfer3D;
use rutabaga_gfx::VulkanInfo;
use rutabaga_gfx::RUTABAGA_FLAG_FENCE;
use rutabaga_gfx::RUTABAGA_FLAG_FENCE_HOST_SHAREABLE;
use rutabaga_gfx::RUTABAGA_HANDLE_TYPE_MEM_SHM;
use rutabaga_gfx::RUTABAGA_MAP_ACCESS_RW;
use rutabaga_gfx::RUTABAGA_MAP_CACHE_CACHED;

const SNAPSHOT_DIR: &str = "/tmp/";

pub struct KumquatGpuConnection {
    stream: RutabagaStream,
}

pub struct KumquatGpuResource {
    attached_contexts: Set<u32>,
    mapping: Option<RutabagaMemoryMapping>,
}

pub struct FenceData {
    pub pending_fences: Map<u64, RutabagaEvent>,
}

pub type FenceState = Arc<Mutex<FenceData>>;

pub fn create_fence_handler(fence_state: FenceState) -> RutabagaFenceHandler {
    RutabagaFenceHandler::new(move |completed_fence: RutabagaFence| {
        let mut state = fence_state.lock().unwrap();
        match (*state).pending_fences.entry(completed_fence.fence_id) {
            Entry::Occupied(o) => {
                let (_, mut event) = o.remove_entry();
                event.signal().unwrap();
            }
            Entry::Vacant(_) => {
                // This is fine, since an actual fence doesn't create emulated sync
                // entry
            }
        }
    })
}

pub struct KumquatGpu {
    rutabaga: Rutabaga,
    fence_state: FenceState,
    id_allocator: u32,
    resources: Map<u32, KumquatGpuResource>,
}

impl KumquatGpu {
    pub fn new(capset_names: String, renderer_features: String) -> RutabagaResult<KumquatGpu> {
        let capset_mask = calculate_capset_mask(capset_names.as_str().split(":"));
        let fence_state = Arc::new(Mutex::new(FenceData {
            pending_fences: Default::default(),
        }));

        let fence_handler = create_fence_handler(fence_state.clone());

        let renderer_features_opt = if renderer_features.is_empty() {
            None
        } else {
            Some(renderer_features)
        };

        let rutabaga = RutabagaBuilder::new(RutabagaComponentType::CrossDomain, capset_mask)
            .set_use_external_blob(true)
            .set_use_egl(true)
            .set_wsi(RutabagaWsi::Surfaceless)
            .set_renderer_features(renderer_features_opt)
            .build(fence_handler, None)?;

        Ok(KumquatGpu {
            rutabaga,
            fence_state,
            id_allocator: 0,
            resources: Default::default(),
        })
    }

    pub fn allocate_id(&mut self) -> u32 {
        self.id_allocator = self.id_allocator + 1;
        self.id_allocator
    }
}

impl KumquatGpuConnection {
    pub fn new(connection: RutabagaTube) -> KumquatGpuConnection {
        KumquatGpuConnection {
            stream: RutabagaStream::new(connection),
        }
    }

    pub fn process_command(&mut self, kumquat_gpu: &mut KumquatGpu) -> RutabagaResult<bool> {
        let mut hung_up = false;
        let protocols = self.stream.read()?;

        for protocol in protocols {
            match protocol {
                KumquatGpuProtocol::GetNumCapsets => {
                    let resp = kumquat_gpu_protocol_ctrl_hdr {
                        type_: KUMQUAT_GPU_PROTOCOL_RESP_NUM_CAPSETS,
                        payload: kumquat_gpu.rutabaga.get_num_capsets(),
                    };

                    self.stream.write(KumquatGpuProtocolWrite::Cmd(resp))?;
                }
                KumquatGpuProtocol::GetCapsetInfo(capset_index) => {
                    let (capset_id, version, size) =
                        kumquat_gpu.rutabaga.get_capset_info(capset_index)?;

                    let resp = kumquat_gpu_protocol_resp_capset_info {
                        hdr: kumquat_gpu_protocol_ctrl_hdr {
                            type_: KUMQUAT_GPU_PROTOCOL_RESP_CAPSET_INFO,
                            ..Default::default()
                        },
                        capset_id,
                        version,
                        size,
                        ..Default::default()
                    };

                    self.stream.write(KumquatGpuProtocolWrite::Cmd(resp))?;
                }
                KumquatGpuProtocol::GetCapset(cmd) => {
                    let capset = kumquat_gpu
                        .rutabaga
                        .get_capset(cmd.capset_id, cmd.capset_version)?;

                    let resp = kumquat_gpu_protocol_ctrl_hdr {
                        type_: KUMQUAT_GPU_PROTOCOL_RESP_CAPSET,
                        payload: capset.len().try_into()?,
                    };

                    self.stream
                        .write(KumquatGpuProtocolWrite::CmdWithData(resp, capset))?;
                }
                KumquatGpuProtocol::CtxCreate(cmd) => {
                    let context_id = kumquat_gpu.allocate_id();
                    let context_name: Option<String> =
                        String::from_utf8(cmd.debug_name.to_vec()).ok();

                    kumquat_gpu.rutabaga.create_context(
                        context_id,
                        cmd.context_init,
                        context_name.as_deref(),
                    )?;

                    let resp = kumquat_gpu_protocol_ctrl_hdr {
                        type_: KUMQUAT_GPU_PROTOCOL_RESP_CONTEXT_CREATE,
                        payload: context_id,
                    };

                    self.stream.write(KumquatGpuProtocolWrite::Cmd(resp))?;
                }
                KumquatGpuProtocol::CtxDestroy(ctx_id) => {
                    kumquat_gpu.rutabaga.destroy_context(ctx_id)?;
                }
                KumquatGpuProtocol::CtxAttachResource(cmd) => {
                    kumquat_gpu
                        .rutabaga
                        .context_attach_resource(cmd.ctx_id, cmd.resource_id)?;
                }
                KumquatGpuProtocol::CtxDetachResource(cmd) => {
                    kumquat_gpu
                        .rutabaga
                        .context_detach_resource(cmd.ctx_id, cmd.resource_id)?;

                    let mut resource = kumquat_gpu
                        .resources
                        .remove(&cmd.resource_id)
                        .ok_or(RutabagaErrorKind::InvalidResourceId)?;

                    resource.attached_contexts.remove(&cmd.ctx_id);
                    if resource.attached_contexts.len() == 0 {
                        if resource.mapping.is_some() {
                            kumquat_gpu.rutabaga.detach_backing(cmd.resource_id)?;
                        }

                        kumquat_gpu.rutabaga.unref_resource(cmd.resource_id)?;
                    } else {
                        kumquat_gpu.resources.insert(cmd.resource_id, resource);
                    }
                }
                KumquatGpuProtocol::ResourceCreate3d(cmd) => {
                    let resource_create_3d = ResourceCreate3D {
                        target: cmd.target,
                        format: cmd.format,
                        bind: cmd.bind,
                        width: cmd.width,
                        height: cmd.height,
                        depth: cmd.depth,
                        array_size: cmd.array_size,
                        last_level: cmd.last_level,
                        nr_samples: cmd.nr_samples,
                        flags: cmd.flags,
                    };

                    let size = cmd.size as usize;
                    let descriptor: RutabagaDescriptor =
                        RutabagaSharedMemory::new("rutabaga_server", size as u64)?.into();

                    let clone = descriptor.try_clone()?;
                    let mut vecs: Vec<RutabagaIovec> = Vec::new();

                    // Creating the mapping closes the cloned descriptor.
                    let mapping = RutabagaMemoryMapping::from_safe_descriptor(
                        clone,
                        size,
                        RUTABAGA_MAP_CACHE_CACHED | RUTABAGA_MAP_ACCESS_RW,
                    )?;
                    let rutabaga_mapping = mapping.as_rutabaga_mapping();

                    vecs.push(RutabagaIovec {
                        base: rutabaga_mapping.ptr as *mut c_void,
                        len: size,
                    });

                    let resource_id = kumquat_gpu.allocate_id();

                    kumquat_gpu
                        .rutabaga
                        .resource_create_3d(resource_id, resource_create_3d)?;

                    kumquat_gpu.rutabaga.attach_backing(resource_id, vecs)?;
                    kumquat_gpu.resources.insert(
                        resource_id,
                        KumquatGpuResource {
                            attached_contexts: Default::default(),
                            mapping: Some(mapping),
                        },
                    );

                    kumquat_gpu
                        .rutabaga
                        .context_attach_resource(cmd.ctx_id, resource_id)?;

                    let resp = kumquat_gpu_protocol_resp_resource_create {
                        hdr: kumquat_gpu_protocol_ctrl_hdr {
                            type_: KUMQUAT_GPU_PROTOCOL_RESP_RESOURCE_CREATE,
                            ..Default::default()
                        },
                        resource_id,
                        ..Default::default()
                    };

                    self.stream.write(KumquatGpuProtocolWrite::CmdWithHandle(
                        resp,
                        RutabagaHandle {
                            os_handle: descriptor,
                            handle_type: RUTABAGA_HANDLE_TYPE_MEM_SHM,
                        },
                    ))?;
                }
                KumquatGpuProtocol::TransferToHost3d(cmd, emulated_fence) => {
                    let resource_id = cmd.resource_id;

                    let transfer = Transfer3D {
                        x: cmd.box_.x,
                        y: cmd.box_.y,
                        z: cmd.box_.z,
                        w: cmd.box_.w,
                        h: cmd.box_.h,
                        d: cmd.box_.d,
                        level: cmd.level,
                        stride: cmd.stride,
                        layer_stride: cmd.layer_stride,
                        offset: cmd.offset,
                    };

                    kumquat_gpu
                        .rutabaga
                        .transfer_write(cmd.ctx_id, resource_id, transfer, None)?;

                    let mut event: RutabagaEvent = emulated_fence.try_into()?;
                    event.signal()?;
                }
                KumquatGpuProtocol::TransferFromHost3d(cmd, emulated_fence) => {
                    let resource_id = cmd.resource_id;

                    let transfer = Transfer3D {
                        x: cmd.box_.x,
                        y: cmd.box_.y,
                        z: cmd.box_.z,
                        w: cmd.box_.w,
                        h: cmd.box_.h,
                        d: cmd.box_.d,
                        level: cmd.level,
                        stride: cmd.stride,
                        layer_stride: cmd.layer_stride,
                        offset: cmd.offset,
                    };

                    kumquat_gpu
                        .rutabaga
                        .transfer_read(cmd.ctx_id, resource_id, transfer, None)?;

                    let mut event: RutabagaEvent = emulated_fence.try_into()?;
                    event.signal()?;
                }
                KumquatGpuProtocol::CmdSubmit3d(cmd, mut cmd_buf, fence_ids) => {
                    kumquat_gpu.rutabaga.submit_command(
                        cmd.ctx_id,
                        &mut cmd_buf[..],
                        &fence_ids[..],
                    )?;

                    if cmd.flags & RUTABAGA_FLAG_FENCE != 0 {
                        let fence_id = kumquat_gpu.allocate_id() as u64;
                        let fence = RutabagaFence {
                            flags: cmd.flags,
                            fence_id,
                            ctx_id: cmd.ctx_id,
                            ring_idx: cmd.ring_idx,
                        };

                        let mut fence_descriptor_opt: Option<RutabagaHandle> = None;
                        let actual_fence = cmd.flags & RUTABAGA_FLAG_FENCE_HOST_SHAREABLE != 0;
                        if !actual_fence {
                            let event: RutabagaEvent = RutabagaEvent::new()?;
                            let clone = event.try_clone()?;
                            let emulated_fence: RutabagaHandle = clone.into();

                            fence_descriptor_opt = Some(emulated_fence);
                            let mut fence_state = kumquat_gpu.fence_state.lock().unwrap();
                            (*fence_state).pending_fences.insert(fence_id, event);
                        }

                        kumquat_gpu.rutabaga.create_fence(fence)?;

                        if actual_fence {
                            fence_descriptor_opt =
                                Some(kumquat_gpu.rutabaga.export_fence(fence_id)?);
                            kumquat_gpu.rutabaga.destroy_fences(&[fence_id])?;
                        }

                        let fence_descriptor = fence_descriptor_opt
                            .context("No fence descriptor")
                            .context(RutabagaErrorKind::SpecViolation)?;

                        let resp = kumquat_gpu_protocol_resp_cmd_submit_3d {
                            hdr: kumquat_gpu_protocol_ctrl_hdr {
                                type_: KUMQUAT_GPU_PROTOCOL_RESP_CMD_SUBMIT_3D,
                                ..Default::default()
                            },
                            fence_id,
                            handle_type: fence_descriptor.handle_type,
                            ..Default::default()
                        };

                        self.stream.write(KumquatGpuProtocolWrite::CmdWithHandle(
                            resp,
                            fence_descriptor,
                        ))?;
                    }
                }
                KumquatGpuProtocol::ResourceCreateBlob(cmd) => {
                    let resource_id = kumquat_gpu.allocate_id();

                    let resource_create_blob = ResourceCreateBlob {
                        blob_mem: cmd.blob_mem,
                        blob_flags: cmd.blob_flags,
                        blob_id: cmd.blob_id,
                        size: cmd.size,
                    };

                    kumquat_gpu.rutabaga.resource_create_blob(
                        cmd.ctx_id,
                        resource_id,
                        resource_create_blob,
                        None,
                        None,
                    )?;

                    let handle = kumquat_gpu.rutabaga.export_blob(resource_id)?;
                    let mut vk_info: VulkanInfo = Default::default();
                    if let Ok(vulkan_info) = kumquat_gpu.rutabaga.vulkan_info(resource_id) {
                        vk_info = vulkan_info;
                    }

                    kumquat_gpu.resources.insert(
                        resource_id,
                        KumquatGpuResource {
                            attached_contexts: Set::from([cmd.ctx_id]),
                            mapping: None,
                        },
                    );

                    let resp = kumquat_gpu_protocol_resp_resource_create {
                        hdr: kumquat_gpu_protocol_ctrl_hdr {
                            type_: KUMQUAT_GPU_PROTOCOL_RESP_RESOURCE_CREATE,
                            ..Default::default()
                        },
                        resource_id,
                        handle_type: handle.handle_type,
                        vulkan_info: vk_info,
                    };

                    self.stream
                        .write(KumquatGpuProtocolWrite::CmdWithHandle(resp, handle))?;

                    kumquat_gpu
                        .rutabaga
                        .context_attach_resource(cmd.ctx_id, resource_id)?;
                }
                KumquatGpuProtocol::SnapshotSave => {
                    kumquat_gpu.rutabaga.snapshot(&Path::new(SNAPSHOT_DIR))?;

                    let resp = kumquat_gpu_protocol_ctrl_hdr {
                        type_: KUMQUAT_GPU_PROTOCOL_RESP_OK_SNAPSHOT,
                        payload: 0,
                    };

                    self.stream.write(KumquatGpuProtocolWrite::Cmd(resp))?;
                }
                KumquatGpuProtocol::SnapshotRestore => {
                    kumquat_gpu.rutabaga.restore(&Path::new(SNAPSHOT_DIR))?;

                    let resp = kumquat_gpu_protocol_ctrl_hdr {
                        type_: KUMQUAT_GPU_PROTOCOL_RESP_OK_SNAPSHOT,
                        payload: 0,
                    };

                    self.stream.write(KumquatGpuProtocolWrite::Cmd(resp))?;
                }
                KumquatGpuProtocol::OkNoData => {
                    hung_up = true;
                }
                _ => {
                    error!("Unsupported protocol {:?}", protocol);
                    return Err(RutabagaErrorKind::Unsupported.into());
                }
            };
        }

        Ok(hung_up)
    }
}

impl AsBorrowedDescriptor for KumquatGpuConnection {
    fn as_borrowed_descriptor(&self) -> &RutabagaDescriptor {
        self.stream.as_borrowed_descriptor()
    }
}
