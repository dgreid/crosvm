// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Objective-C bridge for the macOS display helper process.
// The helper process owns the AppKit main thread. Window creation, drawing,
// and event handling all happen on the main thread via dispatch_async.
// The Rust side runs a background thread that reads from the Tube.

#import <Cocoa/Cocoa.h>
#import <Carbon/Carbon.h>  // for kVK_* key codes

#include <stdint.h>
#include <stdbool.h>

// Callback type for input events from the helper to crosvm.
// type_ and code follow Linux input_event conventions (EV_KEY, EV_REL, etc).
typedef void (*macos_event_callback_t)(
    void *context,
    uint32_t surface_id,
    uint16_t type_,
    uint16_t code,
    int32_t value
);

// Callback for window close requests.
typedef void (*macos_close_callback_t)(void *context, uint32_t surface_id);

// Global event callbacks (set once at init).
static macos_event_callback_t g_event_callback = NULL;
static void *g_event_context = NULL;
static macos_close_callback_t g_close_callback = NULL;
static void *g_close_context = NULL;

// Forward declarations.
uint16_t macos_keycode_to_linux(uint16_t mac_keycode);
@class HelperWindowDelegate;

// ---------- FramebufferView ----------

@interface FramebufferView : NSView
@property (nonatomic) const uint8_t *framebuffer;
@property (nonatomic) uint32_t fbWidth;
@property (nonatomic) uint32_t fbHeight;
@property (nonatomic) uint32_t surfaceId;
@property (nonatomic, strong) HelperWindowDelegate *windowDelegate;
@end

@implementation FramebufferView

- (BOOL)isFlipped { return YES; }
- (BOOL)acceptsFirstResponder { return YES; }

- (void)drawRect:(NSRect)dirtyRect {
    if (!self.framebuffer) return;

    CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
    // XRGB8888: 4 bytes per pixel, B G R X in memory (little-endian).
    CGContextRef bitmapCtx = CGBitmapContextCreate(
        (void *)self.framebuffer,
        self.fbWidth, self.fbHeight,
        8, self.fbWidth * 4, cs,
        kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little);
    CGColorSpaceRelease(cs);
    if (!bitmapCtx) return;

    CGImageRef image = CGBitmapContextCreateImage(bitmapCtx);
    CGContextRelease(bitmapCtx);
    if (!image) return;

    CGContextRef drawCtx = [[NSGraphicsContext currentContext] CGContext];
    // Flip vertically: NSView with isFlipped=YES has origin at top-left,
    // but CGContextDrawImage draws with origin at bottom-left.
    CGContextSaveGState(drawCtx);
    CGContextTranslateCTM(drawCtx, 0, self.fbHeight);
    CGContextScaleCTM(drawCtx, 1.0, -1.0);
    CGContextDrawImage(drawCtx,
                       CGRectMake(0, 0, self.fbWidth, self.fbHeight),
                       image);
    CGContextRestoreGState(drawCtx);
    CGImageRelease(image);
}

- (void)keyDown:(NSEvent *)event {
    if (!g_event_callback) return;
    uint16_t code = macos_keycode_to_linux([event keyCode]);
    if (code == 0 && [event keyCode] != 0) return;
    // EV_KEY = 1, value 1 = press
    g_event_callback(g_event_context, self.surfaceId, 1, code, 1);
}

- (void)keyUp:(NSEvent *)event {
    if (!g_event_callback) return;
    uint16_t code = macos_keycode_to_linux([event keyCode]);
    if (code == 0 && [event keyCode] != 0) return;
    // EV_KEY = 1, value 0 = release
    g_event_callback(g_event_context, self.surfaceId, 1, code, 0);
}

- (void)mouseMoved:(NSEvent *)event {
    if (!g_event_callback) return;
    NSPoint loc = [self convertPoint:[event locationInWindow] fromView:nil];
    // EV_ABS = 3, ABS_X = 0, ABS_Y = 1
    g_event_callback(g_event_context, self.surfaceId, 3, 0, (int32_t)loc.x);
    g_event_callback(g_event_context, self.surfaceId, 3, 1, (int32_t)loc.y);
}

- (void)mouseDragged:(NSEvent *)event {
    [self mouseMoved:event];
}

- (void)mouseDown:(NSEvent *)event {
    if (!g_event_callback) return;
    // EV_KEY = 1, BTN_LEFT = 0x110, value 1 = press
    g_event_callback(g_event_context, self.surfaceId, 1, 0x110, 1);
}

- (void)mouseUp:(NSEvent *)event {
    if (!g_event_callback) return;
    // EV_KEY = 1, BTN_LEFT = 0x110, value 0 = release
    g_event_callback(g_event_context, self.surfaceId, 1, 0x110, 0);
}

- (void)rightMouseDown:(NSEvent *)event {
    if (!g_event_callback) return;
    // BTN_RIGHT = 0x111
    g_event_callback(g_event_context, self.surfaceId, 1, 0x111, 1);
}

- (void)rightMouseUp:(NSEvent *)event {
    if (!g_event_callback) return;
    g_event_callback(g_event_context, self.surfaceId, 1, 0x111, 0);
}

- (void)scrollWheel:(NSEvent *)event {
    if (!g_event_callback) return;
    // EV_REL = 2, REL_WHEEL = 8
    int32_t dy = (int32_t)[event scrollingDeltaY];
    if (dy != 0) {
        g_event_callback(g_event_context, self.surfaceId, 2, 8, dy);
    }
}

- (void)flagsChanged:(NSEvent *)event {
    if (!g_event_callback) return;
    NSEventModifierFlags flags = [event modifierFlags];
    uint16_t keyCode = [event keyCode];
    uint16_t code = macos_keycode_to_linux(keyCode);
    if (code == 0 && keyCode != 0) return;

    // Determine press/release from whether the modifier flag is set.
    BOOL pressed = NO;
    switch (keyCode) {
        case 0x38: // kVK_Shift
        case 0x3C: // kVK_RightShift
            pressed = (flags & NSEventModifierFlagShift) != 0;
            break;
        case 0x3B: // kVK_Control
        case 0x3E: // kVK_RightControl
            pressed = (flags & NSEventModifierFlagControl) != 0;
            break;
        case 0x3A: // kVK_Option
        case 0x3D: // kVK_RightOption
            pressed = (flags & NSEventModifierFlagOption) != 0;
            break;
        case 0x37: // kVK_Command
        case 0x36: // kVK_RightCommand
            pressed = (flags & NSEventModifierFlagCommand) != 0;
            break;
        case 0x39: // kVK_CapsLock
            pressed = (flags & NSEventModifierFlagCapsLock) != 0;
            break;
        default:
            return;
    }
    g_event_callback(g_event_context, self.surfaceId, 1, code, pressed ? 1 : 0);
}

- (void)rightMouseDragged:(NSEvent *)event {
    [self mouseMoved:event];
}

@end

// ---------- WindowDelegate ----------

@interface HelperWindowDelegate : NSObject <NSWindowDelegate>
@property (nonatomic) uint32_t surfaceId;
@end

@implementation HelperWindowDelegate

- (BOOL)windowShouldClose:(id)sender {
    if (g_close_callback) {
        g_close_callback(g_close_context, self.surfaceId);
    }
    return NO;  // Don't actually close; let crosvm decide.
}

@end

// ---------- macOS keycode to Linux keycode ----------

// Maps macOS virtual key codes (kVK_*) to Linux KEY_* codes.
uint16_t macos_keycode_to_linux(uint16_t mac_keycode) {
    // Table covers the most common keys. Returns 0 for unknown.
    static const uint16_t table[128] = {
        [0x00] = 30,   // kVK_ANSI_A -> KEY_A
        [0x01] = 31,   // kVK_ANSI_S -> KEY_S
        [0x02] = 32,   // kVK_ANSI_D -> KEY_D
        [0x03] = 33,   // kVK_ANSI_F -> KEY_F
        [0x04] = 35,   // kVK_ANSI_H -> KEY_H
        [0x05] = 34,   // kVK_ANSI_G -> KEY_G
        [0x06] = 44,   // kVK_ANSI_Z -> KEY_Z
        [0x07] = 45,   // kVK_ANSI_X -> KEY_X
        [0x08] = 46,   // kVK_ANSI_C -> KEY_C
        [0x09] = 47,   // kVK_ANSI_V -> KEY_V
        [0x0B] = 48,   // kVK_ANSI_B -> KEY_B
        [0x0C] = 16,   // kVK_ANSI_Q -> KEY_Q
        [0x0D] = 17,   // kVK_ANSI_W -> KEY_W
        [0x0E] = 18,   // kVK_ANSI_E -> KEY_E
        [0x0F] = 19,   // kVK_ANSI_R -> KEY_R
        [0x10] = 21,   // kVK_ANSI_Y -> KEY_Y
        [0x11] = 20,   // kVK_ANSI_T -> KEY_T
        [0x12] = 2,    // kVK_ANSI_1 -> KEY_1
        [0x13] = 3,    // kVK_ANSI_2 -> KEY_2
        [0x14] = 4,    // kVK_ANSI_3 -> KEY_3
        [0x15] = 5,    // kVK_ANSI_4 -> KEY_4
        [0x16] = 7,    // kVK_ANSI_6 -> KEY_6
        [0x17] = 6,    // kVK_ANSI_5 -> KEY_5
        [0x18] = 13,   // kVK_ANSI_Equal -> KEY_EQUAL
        [0x19] = 10,   // kVK_ANSI_9 -> KEY_9
        [0x1A] = 8,    // kVK_ANSI_7 -> KEY_7
        [0x1B] = 12,   // kVK_ANSI_Minus -> KEY_MINUS
        [0x1C] = 9,    // kVK_ANSI_8 -> KEY_8
        [0x1D] = 11,   // kVK_ANSI_0 -> KEY_0
        [0x1E] = 27,   // kVK_ANSI_RightBracket -> KEY_RIGHTBRACE
        [0x1F] = 24,   // kVK_ANSI_O -> KEY_O
        [0x20] = 22,   // kVK_ANSI_U -> KEY_U
        [0x21] = 26,   // kVK_ANSI_LeftBracket -> KEY_LEFTBRACE
        [0x22] = 23,   // kVK_ANSI_I -> KEY_I
        [0x23] = 25,   // kVK_ANSI_P -> KEY_P
        [0x24] = 28,   // kVK_Return -> KEY_ENTER
        [0x25] = 38,   // kVK_ANSI_L -> KEY_L
        [0x26] = 36,   // kVK_ANSI_J -> KEY_J
        [0x27] = 40,   // kVK_ANSI_Quote -> KEY_APOSTROPHE
        [0x28] = 37,   // kVK_ANSI_K -> KEY_K
        [0x29] = 39,   // kVK_ANSI_Semicolon -> KEY_SEMICOLON
        [0x2A] = 43,   // kVK_ANSI_Backslash -> KEY_BACKSLASH
        [0x2B] = 51,   // kVK_ANSI_Comma -> KEY_COMMA
        [0x2C] = 53,   // kVK_ANSI_Slash -> KEY_SLASH
        [0x2D] = 49,   // kVK_ANSI_N -> KEY_N
        [0x2E] = 50,   // kVK_ANSI_M -> KEY_M
        [0x2F] = 52,   // kVK_ANSI_Period -> KEY_DOT
        [0x30] = 15,   // kVK_Tab -> KEY_TAB
        [0x31] = 57,   // kVK_Space -> KEY_SPACE
        [0x32] = 41,   // kVK_ANSI_Grave -> KEY_GRAVE
        [0x33] = 14,   // kVK_Delete (backspace) -> KEY_BACKSPACE
        [0x35] = 1,    // kVK_Escape -> KEY_ESC
        [0x36] = 126,  // kVK_RightCommand -> KEY_RIGHTMETA
        [0x37] = 125,  // kVK_Command -> KEY_LEFTMETA
        [0x38] = 42,   // kVK_Shift -> KEY_LEFTSHIFT
        [0x39] = 58,   // kVK_CapsLock -> KEY_CAPSLOCK
        [0x3A] = 56,   // kVK_Option -> KEY_LEFTALT
        [0x3B] = 29,   // kVK_Control -> KEY_LEFTCTRL
        [0x3C] = 54,   // kVK_RightShift -> KEY_RIGHTSHIFT
        [0x3D] = 100,  // kVK_RightOption -> KEY_RIGHTALT
        [0x3E] = 97,   // kVK_RightControl -> KEY_RIGHTCTRL
        [0x60] = 63,   // kVK_F5 -> KEY_F5
        [0x61] = 64,   // kVK_F6 -> KEY_F6
        [0x62] = 65,   // kVK_F7 -> KEY_F7
        [0x63] = 61,   // kVK_F3 -> KEY_F3
        [0x64] = 66,   // kVK_F8 -> KEY_F8
        [0x65] = 67,   // kVK_F9 -> KEY_F9
        [0x67] = 87,   // kVK_F11 -> KEY_F11
        [0x69] = 99,   // kVK_F13 -> KEY_SYSRQ (PrintScreen)
        [0x6D] = 68,   // kVK_F10 -> KEY_F10
        [0x6F] = 88,   // kVK_F12 -> KEY_F12
        [0x73] = 102,  // kVK_Home -> KEY_HOME
        [0x74] = 104,  // kVK_PageUp -> KEY_PAGEUP
        [0x75] = 111,  // kVK_ForwardDelete -> KEY_DELETE
        [0x76] = 62,   // kVK_F4 -> KEY_F4
        [0x77] = 107,  // kVK_End -> KEY_END
        [0x78] = 60,   // kVK_F2 -> KEY_F2
        [0x79] = 109,  // kVK_PageDown -> KEY_PAGEDOWN
        [0x7A] = 59,   // kVK_F1 -> KEY_F1
        [0x7B] = 105,  // kVK_LeftArrow -> KEY_LEFT
        [0x7C] = 106,  // kVK_RightArrow -> KEY_RIGHT
        [0x7D] = 108,  // kVK_DownArrow -> KEY_DOWN
        [0x7E] = 103,  // kVK_UpArrow -> KEY_UP
    };

    if (mac_keycode >= 128) return 0;
    return table[mac_keycode];
}

// ---------- Public C interface ----------

void macos_helper_init_app(void) {
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
}

void macos_helper_run_app(void) {
    [NSApp activateIgnoringOtherApps:YES];
    [NSApp run];
}

void macos_helper_stop_app(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSApp stop:nil];
        // Post a dummy event to wake the run loop so it processes the stop.
        NSEvent *event = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                            location:NSZeroPoint
                                       modifierFlags:0
                                           timestamp:0
                                        windowNumber:0
                                             context:nil
                                             subtype:0
                                               data1:0
                                               data2:0];
        [NSApp postEvent:event atStart:YES];
    });
}

void macos_helper_set_event_callback(macos_event_callback_t callback, void *context) {
    g_event_callback = callback;
    g_event_context = context;
}

void macos_helper_set_close_callback(macos_close_callback_t callback, void *context) {
    g_close_callback = callback;
    g_close_context = context;
}

void *macos_helper_create_window(uint32_t surface_id, uint32_t width,
                                  uint32_t height, uint8_t *framebuffer) {
    NSRect frame = NSMakeRect(100, 100, width, height);
    NSUInteger style = NSWindowStyleMaskTitled |
                       NSWindowStyleMaskClosable |
                       NSWindowStyleMaskMiniaturizable;
    NSWindow *window = [[NSWindow alloc] initWithContentRect:frame
                                                   styleMask:style
                                                     backing:NSBackingStoreBuffered
                                                       defer:NO];
    [window setTitle:@"crosvm"];
    [window setReleasedWhenClosed:NO];

    // Accept mouse-moved events.
    [window setAcceptsMouseMovedEvents:YES];

    FramebufferView *view = [[FramebufferView alloc] initWithFrame:frame];
    view.framebuffer = framebuffer;
    view.fbWidth = width;
    view.fbHeight = height;
    view.surfaceId = surface_id;
    [window setContentView:view];

    HelperWindowDelegate *delegate = [[HelperWindowDelegate alloc] init];
    delegate.surfaceId = surface_id;
    [window setDelegate:delegate];
    view.windowDelegate = delegate;

    [window makeKeyAndOrderFront:nil];

    // Return the window as an opaque handle. ARC retains it via the view.
    return (__bridge_retained void *)window;
}

void macos_helper_destroy_window(void *handle) {
    if (!handle) return;
    NSWindow *window = (__bridge_transfer NSWindow *)handle;
    [window close];
}

void macos_helper_flip(void *handle) {
    if (!handle) return;
    NSWindow *window = (__bridge NSWindow *)handle;
    NSView *view = [window contentView];
    [view setNeedsDisplay:YES];
}

void macos_helper_inject_key(void *handle, uint16_t keycode, bool key_down) {
    if (!handle) return;
    NSWindow *window = (__bridge NSWindow *)handle;
    NSView *view = [window contentView];
    NSEventType type = key_down ? NSEventTypeKeyDown : NSEventTypeKeyUp;
    NSEvent *event = [NSEvent keyEventWithType:type
                                      location:NSZeroPoint
                                 modifierFlags:0
                                     timestamp:0
                                  windowNumber:[window windowNumber]
                                       context:nil
                                    characters:@""
                   charactersIgnoringModifiers:@""
                                     isARepeat:NO
                                       keyCode:keycode];
    if (key_down) {
        [view keyDown:event];
    } else {
        [view keyUp:event];
    }
}
