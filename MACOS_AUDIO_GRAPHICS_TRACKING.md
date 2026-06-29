# macOS Audio & Graphics — Tracking

## What shipped (this branch)

### Audio (virtio-snd + CoreAudio)

| Commit | What |
|--------|------|
| `891f63198` | `coreaudio` crate: SPSC ring buffer, format conversion, AudioUnit playback, StreamSource impl |
| `2265fbcc8` | `devices/snd/sys/macos.rs`: COREAUDIO backend variant, stream source generators, buffer reader/writer |
| `f3b271c58` | `macos.rs`: VirtioSnd device wired into VM with MMIO + IRQ + FDT |

### Graphics (virtio-gpu + native display)

| Commit | What |
|--------|------|
| `ace7d70fa` | `gpu_display`: ObjC bridge (.m), DisplayMacos, open_macos(), fixed import_event_device |
| `ee4dca680` | `devices/gpu/mod.rs`: DisplayBackend::MacOs variant |
| `f3b271c58` | `macos.rs`: Gpu device wired into VM with MacOs display, MMIO + IRQ + FDT |

---

## What still needs to happen

### P0 — Required for functional audio/graphics

#### Display helper process (AppKit main thread solution)
**Status:** IPC framework complete, AppKit windowing TODO
**Why:** AppKit requires all UI operations on thread 0. crosvm's main thread blocks on
`vcpu_threads[0].join()`. Solution: separate helper process owns AppKit main thread.

**Done:**
- IPC protocol: `DisplayRequest`/`DisplayResponse` enums over Tube (JSON + SCM_RIGHTS)
- `DisplayMacos` (DisplayT impl) spawns helper via `crosvm display-helper` subcommand
- SharedMemory framebuffer: crosvm creates shm, mmaps it, sends fd to helper via Tube
- `MacosSurface::flip()` sends Flip message to helper
- Helper receives CreateSurface, maps shm, acknowledges
- Helper handles Shutdown, DestroySurface, Tube EOF → clean exit
- 9 unit tests covering protocol roundtrip, fd passing, shared memory visibility
- AI design review + implementation review incorporated

**Remaining:**
1. Add AppKit windowing in helper: create NSWindow, blit from shm to CALayer on Flip
2. Wire `AsRawDescriptor` to Tube fd (not Event fd) so WaitContext wakes on responses
3. Forward input events (keyboard/mouse) from helper → crosvm via DisplayResponse
4. Handle CloseRequested → set surface flag

#### Runtime validation — audio
**Status:** Not started
**Why:** The code compiles and unit tests pass, but nobody has booted a VM with audio yet.

**What to test:**
1. Boot VM, `aplay -l` shows virtio sound card
2. `aplay test.wav` produces audible output on host
3. No crashes or hangs during playback
4. Silence when no audio playing (no static/noise from underruns)

#### Runtime validation — graphics
**Status:** Not started
**Why:** Same — compiles but untested at runtime.

**What to test:**
1. Boot VM, `dmesg | grep virtio` shows GPU device detected
2. Guest loads virtio-gpu DRM driver
3. With main thread fix: window appears on host with console output visible

### P1 — Important for usability

#### Keyboard input forwarding
**Status:** Not started
**Why:** Without this, the display window is view-only.

**What to do:**
1. In the ObjC bridge, intercept NSEvent key events in the poll loop
2. Translate NSEvent keycodes to Linux input_event codes
3. Return events via `handle_next_event()` → `GpuDisplayEvents`
4. May need a keycode_converter module for macOS (similar to X11/Windows ones)

**Files:** `gpu_display/src/gpu_display_macos_bridge.m`, `gpu_display/src/gpu_display_macos.rs`

#### Mouse input forwarding
**Status:** Not started
**Similar to keyboard** — translate NSEvent mouse events to virtio input events.

#### Audio capture (microphone)
**Status:** Not started
**Why:** Only playback is implemented. Capture requires a reversed SPSC
(CoreAudio input callback writes, executor reads) plus HAL input AudioUnit setup.

**What to do:**
1. Add `capture.rs` to `coreaudio/` crate
2. Implement `new_capture_stream()` / `new_async_capture_stream()` on `CoreAudioStreamSource`
3. Use `kAudioUnitSubType_HALOutput` with input enabled for capture AudioUnit

#### StreamControl pause/resume
**Status:** Not started (CoreAudioStreamControl is a no-op)
**Why:** Audio starts playing immediately and can't be paused. Low priority since the guest controls playback, and underruns produce silence.

**What to do:** Store the AudioUnit in CoreAudioStreamControl, implement `pause()` → `AudioOutputUnitStop()` and `resume()` → `AudioOutputUnitStart()`.

### P2 — Nice to have

#### HiDPI / Retina support
Use `backingScaleFactor` to render at native resolution on Retina displays instead of pixel-doubled.

#### Window resize
Currently fixed framebuffer size. Would need to recreate the surface or handle resize events.

#### Multi-scanout
Support multiple display outputs.

#### Cursor surface
Hardware cursor overlay (separate virtio-gpu resource for cursor).

#### Audio device selection
Currently uses system default output. Could expose device selection via parameters.

#### Audio device hot-plug
Switching headphones mid-stream may break. Would need AudioUnit notification callbacks.

---

## Known limitations (documented, will not fix soon)

- No 3D acceleration (VirglRenderer/Gfxstream are Linux-only)
- No dmabuf/import_resource on macOS (CPU framebuffer path only)
- No sample rate conversion beyond CoreAudio's internal resampler
- No channel mapping beyond CoreAudio's internal mixer
- No window close → guest shutdown (window close just hides window)
- `AudioOutputUnitStop` synchronicity assumption — we assume it waits for in-flight callbacks (observed but not documented by Apple)
