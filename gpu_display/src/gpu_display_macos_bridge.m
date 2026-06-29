// Copyright 2025 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Objective-C bridge for macOS GPU display.
// Provides C-callable functions for AppKit window management.

#import <Cocoa/Cocoa.h>
#import <QuartzCore/QuartzCore.h>

// Per-surface state
typedef struct {
    NSWindow *window;
    NSView *content_view;
    uint32_t width;
    uint32_t height;
    uint8_t *framebuffer;
    size_t framebuffer_len;
    bool close_requested;
} MacosSurface;

// Display state
typedef struct {
    bool initialized;
} MacosDisplay;

static bool app_initialized = false;

void macos_display_ensure_app(void) {
    if (app_initialized) return;
    // NSApplication must be initialized on the main thread, but
    // sharedApplication can be called from any thread to create it.
    // The actual event processing happens via macos_display_poll_events.
    [NSApplication sharedApplication];
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
    app_initialized = true;
}

void *macos_display_create(void) {
    macos_display_ensure_app();
    MacosDisplay *display = calloc(1, sizeof(MacosDisplay));
    display->initialized = true;
    return display;
}

void macos_display_destroy(void *display_ptr) {
    if (!display_ptr) return;
    free(display_ptr);
}

void *macos_surface_create(uint32_t width, uint32_t height) {
    MacosSurface *surface = calloc(1, sizeof(MacosSurface));
    surface->width = width;
    surface->height = height;
    surface->close_requested = false;

    size_t bytes_per_pixel = 4; // XRGB8888
    surface->framebuffer_len = (size_t)width * height * bytes_per_pixel;
    surface->framebuffer = calloc(1, surface->framebuffer_len);

    // Window creation must happen on the main thread
    dispatch_async(dispatch_get_main_queue(), ^{
        NSRect frame = NSMakeRect(100, 100, width, height);
        NSUInteger style = NSWindowStyleMaskTitled |
                           NSWindowStyleMaskClosable |
                           NSWindowStyleMaskMiniaturizable;
        surface->window = [[NSWindow alloc] initWithContentRect:frame
                                                      styleMask:style
                                                        backing:NSBackingStoreBuffered
                                                          defer:NO];
        [surface->window setTitle:@"crosvm"];
        [surface->window setReleasedWhenClosed:NO];
        surface->content_view = [surface->window contentView];
        [surface->content_view setWantsLayer:YES];
        [surface->window makeKeyAndOrderFront:nil];
        [NSApp activateIgnoringOtherApps:YES];
    });

    return surface;
}

void macos_surface_destroy(void *surface_ptr) {
    if (!surface_ptr) return;
    MacosSurface *surface = (MacosSurface *)surface_ptr;

    if (surface->window) {
        NSWindow *window = surface->window;
        dispatch_async(dispatch_get_main_queue(), ^{
            [window close];
        });
    }

    free(surface->framebuffer);
    free(surface);
}

uint8_t *macos_surface_framebuffer(void *surface_ptr, size_t *out_len) {
    if (!surface_ptr) return NULL;
    MacosSurface *surface = (MacosSurface *)surface_ptr;
    if (out_len) *out_len = surface->framebuffer_len;
    return surface->framebuffer;
}

uint32_t macos_surface_stride(void *surface_ptr) {
    if (!surface_ptr) return 0;
    MacosSurface *surface = (MacosSurface *)surface_ptr;
    return surface->width * 4;
}

void macos_surface_flip(void *surface_ptr) {
    if (!surface_ptr) return;
    MacosSurface *surface = (MacosSurface *)surface_ptr;

    if (!surface->window || !surface->content_view) return;

    uint32_t w = surface->width;
    uint32_t h = surface->height;
    size_t len = surface->framebuffer_len;

    // Copy framebuffer for thread-safe handoff to main thread
    uint8_t *copy = malloc(len);
    if (!copy) return;
    memcpy(copy, surface->framebuffer, len);

    NSView *view = surface->content_view;
    dispatch_async(dispatch_get_main_queue(), ^{
        CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
        CGContextRef ctx = CGBitmapContextCreate(
            copy, w, h, 8, w * 4, cs,
            kCGImageAlphaNoneSkipFirst | kCGBitmapByteOrder32Little);
        CGColorSpaceRelease(cs);

        if (ctx) {
            CGImageRef image = CGBitmapContextCreateImage(ctx);
            CGContextRelease(ctx);
            if (image) {
                view.layer.contents = (__bridge id)image;
                CGImageRelease(image);
            }
        }
        free(copy);
    });
}

bool macos_surface_close_requested(void *surface_ptr) {
    if (!surface_ptr) return false;
    MacosSurface *surface = (MacosSurface *)surface_ptr;
    return surface->close_requested;
}

void macos_display_poll_events(void) {
    // Drain pending AppKit events without blocking
    @autoreleasepool {
        NSEvent *event;
        while ((event = [NSApp nextEventMatchingMask:NSEventMaskAny
                                           untilDate:nil
                                              inMode:NSDefaultRunLoopMode
                                             dequeue:YES])) {
            [NSApp sendEvent:event];
        }
    }
}

void macos_display_run_event_loop(void) {
    // Run the AppKit event loop on the main thread.
    // This blocks until [NSApp stop:] is called.
    [NSApp run];
}

void macos_display_stop_event_loop(void) {
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSApp stop:nil];
        // Post a dummy event to wake the event loop so it processes the stop
        NSEvent *event = [NSEvent otherEventWithType:NSEventTypeApplicationDefined
                                            location:NSMakePoint(0, 0)
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
