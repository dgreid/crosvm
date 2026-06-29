# macOS Display Helper Process — Design

## Problem

AppKit requires all UI operations on the main thread (thread 0). crosvm's main thread
blocks on `vcpu_threads.join()` after VM startup. We cannot restructure crosvm's generic
main loop without disturbing shared code.

## Solution

A separate helper process (`crosvm_display_helper`) owns thread 0 for AppKit. crosvm
communicates with it via shared memory (framebuffer data) and a Tube (control messages
and input events).

## Architecture

```
crosvm process                          display helper process
┌──────────────────┐                   ┌──────────────────────┐
│ virtio-gpu device│                   │ main thread (0)      │
│                  │                   │   [NSApp run]        │
│ DisplayMacos     │    Tube           │   NSWindow per       │
│  (DisplayT impl) │◄──────────────►  │    surface           │
│                  │  control+events   │   Blit from shm      │
│ SharedMemory ────│──── mmap ────────►│   on flip            │
│  (framebuffer)   │                   │                      │
│                  │                   │ bg thread            │
│                  │                   │   Tube reader        │
│                  │                   │   dispatch_async to  │
│                  │                   │   main thread        │
└──────────────────┘                   └──────────────────────┘
```

## IPC Protocol

### Shared Memory

One SharedMemory region per surface, sized `width * height * 4` (XRGB8888).
Created by crosvm, fd passed to helper via Tube (SCM_RIGHTS).

### Messages (crosvm → helper)

```rust
enum DisplayRequest {
    CreateSurface {
        surface_id: u32,
        width: u32,
        height: u32,
        // SharedMemory fd sent alongside via SCM_RIGHTS
    },
    DestroySurface {
        surface_id: u32,
    },
    Flip {
        surface_id: u32,
    },
    Shutdown,
}
```

### Messages (helper → crosvm)

```rust
enum DisplayResponse {
    SurfaceCreated {
        surface_id: u32,
    },
    CloseRequested {
        surface_id: u32,
    },
    InputEvents {
        surface_id: u32,
        events: Vec<virtio_input_event>,
    },
    Error {
        message: String,
    },
}
```

## crosvm Side: `DisplayMacos` (implements `DisplayT`)

### Construction (`GpuDisplay::open_macos()`)
1. Create socketpair
2. `Command::new("crosvm_display_helper").arg(helper_fd).spawn()`
3. Wrap crosvm-side socket in Tube
4. Store Tube + WaitContext (watches Tube fd for readability)

### `create_surface()`
1. Create `SharedMemory` of `width * height * 4` bytes
2. `MemoryMapping::from_descriptor()` to get local mmap
3. Send `DisplayRequest::CreateSurface` with shm fd via Tube
4. Wait for `DisplayResponse::SurfaceCreated`
5. Return `MacosSurface { mmap, width, height }`

### `MacosSurface::framebuffer()`
Return `GpuDisplayFramebuffer` pointing to the mmap'd region.

### `MacosSurface::flip()`
Send `DisplayRequest::Flip { surface_id }` via Tube.
Helper blits from shared memory to NSWindow backing store.

### `pending_events()` / `handle_next_event()`
Check Tube for `DisplayResponse::InputEvents` or `DisplayResponse::CloseRequested`.
Convert to `GpuDisplayEvents` / set `close_requested` flag.

### `AsRawDescriptor`
Return the WaitContext fd (watches Tube readability).

## Helper Side: `crosvm_display_helper`

### Startup
1. Receive fd from argv (or env var)
2. Wrap in Tube
3. Spawn background thread to read Tube
4. On main thread: `[NSApp run]`

### Background Thread
Reads `DisplayRequest` messages from Tube:
- `CreateSurface`: dispatch_async to main thread → create NSWindow + CALayer,
  mmap the received shm fd, store mapping
- `Flip`: dispatch_async to main thread → `setContentsChanged` / `setNeedsDisplay`
  on CALayer (backed by shared memory data via CGDataProvider)
- `DestroySurface`: dispatch_async → close NSWindow
- `Shutdown`: dispatch_async → `[NSApp terminate:nil]`

### Input Events
NSWindow delegate methods (keyDown, mouseMoved, etc.) translate NSEvent
to `virtio_input_event`, send `DisplayResponse::InputEvents` on Tube.

### Window Close
`windowShouldClose:` sends `DisplayResponse::CloseRequested`.

## Binary Target

Add `src/crosvm_display_helper/main.rs` as a `[[bin]]` target in the workspace,
or a separate crate `display_helper/`. It only builds on macOS.

Needs codesigning but NOT the hypervisor entitlement (no HVF usage).

## Why This Design

- **Fits crosvm's process-per-device model** — helper is just another device process
- **No changes to generic crosvm main loop** — all macOS-specific
- **Tube + SharedMemory are proven primitives** in crosvm's codebase
- **SharedMemory avoids copying framebuffer data** — helper mmaps the same region
- **Input events use existing virtio_input_event type** — no new event format

## Testing Strategy

### Unit Tests (no GUI required)
- Message serialization/deserialization roundtrip
- SharedMemory creation + mmap for framebuffer
- DisplayMacos surface creation with mock Tube
- Flip sends correct message

### Integration Tests (requires macOS GUI session)
- Spawn helper, create surface, flip, verify no crash
- Window close generates CloseRequested
- Multiple surfaces
- Helper shutdown on Tube close

## Open Questions

1. **Helper binary location**: Should crosvm look in its own directory, PATH, or a hardcoded path?
   → Recommend: same directory as the crosvm binary (`std::env::current_exe()` parent dir)
2. **fd passing**: Use Tube's SCM_RIGHTS (automatic with serde), or pass fd number as argv?
   → Recommend: pass the socketpair fd as argv (simple, no bootstrap Tube needed for the initial fd);
   send SharedMemory fds via Tube after connection
3. **Entitlements**: Helper needs no hypervisor entitlement but may need network or other?
   → No special entitlements needed for AppKit window creation
