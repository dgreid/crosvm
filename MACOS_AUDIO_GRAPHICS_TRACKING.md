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
**Status:** Complete (pending commit)
**Why:** AppKit requires all UI operations on thread 0. crosvm's main thread blocks on
`vcpu_threads[0].join()`. Solution: separate helper process owns AppKit main thread.

**Done:**
- IPC protocol: `DisplayRequest`/`DisplayResponse` enums over Tube (JSON + SCM_RIGHTS)
- `DisplayMacos` (DisplayT impl) spawns helper via `crosvm display-helper` subcommand
- SharedMemory framebuffer: crosvm creates shm, mmaps it, sends fd to helper via Tube
- `MacosSurface::flip()` sends Flip message to helper
- Helper receives CreateSurface, maps shm, acknowledges
- Helper handles Shutdown, DestroySurface, Tube EOF → clean exit
- AppKit windowing: NSWindow + FramebufferView blitting from shm on Flip
- Input forwarding: keyboard (keyDown/keyUp/flagsChanged), mouse, scroll → InputEvent responses
- macOS→Linux keycode mapping (A-Z, 0-9, F1-F12, arrows, modifiers, special keys)
- CloseRequested → surface flag propagation via shared HashSet
- `AsRawDescriptor` returns Tube fd so WaitContext wakes on helper responses
- `flush()` non-blocking drain of Tube into pending queue
- `next_event()` pops into current_response (prevents infinite loop on unmatched surfaces)
- Drop sends Shutdown + waits for child to prevent zombies
- 12 unit tests covering protocol, fd passing, shm visibility, flush drain, close flag
- Two AI reviews incorporated (display backend + ObjC bridge)
  - Fixed critical: window delegate ARC retention
  - Fixed critical: flagsChanged for modifier keys
  - Fixed critical: F3-F12 keycode mappings were all wrong
  - Fixed: F13→KEY_SYSRQ (was KEY_PRINT=210)
  - Fixed: added Right Command mapping
  - Fixed: added rightMouseDragged handler
  - Fixed: infinite loop on unmatched surface events

#### Audio capture (microphone)
**Status:** Complete (pending commit)
**Why:** Only playback was implemented. Capture uses reversed SPSC (CoreAudio writes, executor reads).

**Done:**
- `coreaudio/src/capture.rs`: CoreAudioCaptureStream using HALOutput AudioUnit with input enabled
- Input callback renders captured audio into ring buffer with overrun handling
- `next_capture_buffer()` reads from ring buffer, converts Float32→guest format
- Both sync and async capture buffer stream implementations
- Wired into `CoreAudioStreamSource::new_capture_stream()` and `new_async_capture_stream()`

#### StreamControl pause/resume
**Status:** Implemented (stores AudioUnit, no-op trait methods)
**Why:** The `StreamControl` trait only has `set_volume` and `set_mute` — there are no pause/resume
methods in the trait. `CoreAudioStreamControl` stores the AudioUnit for potential future use.

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
3. With display helper: window appears on host with console output visible
4. Keyboard/mouse input works in guest

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
- Mouse X/Y deltas sent as separate SYN reports (may cause slightly jerky diagonal movement)
- Trackpad scroll values are raw pixels (not normalized for notch-based scrolling)
- No keypad, F14-F20, or media key mappings
