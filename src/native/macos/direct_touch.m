// AppKit overlay windows and the macOS 27 direct-touch bridge.
// Cocoa and SDL handles stay inside Objective-C. Rust passes scalar IDs.
#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#include <stdbool.h>
#include <stdint.h>
#include <math.h>

typedef struct SDL_Window SDL_Window;
extern uint32_t SDL_CreateProperties(void);
extern void SDL_DestroyProperties(uint32_t);
extern bool SDL_SetNumberProperty(uint32_t, const char *, int64_t);
extern bool SDL_SetPointerProperty(uint32_t, const char *, void *);
extern SDL_Window *SDL_CreateWindowWithProperties(uint32_t);
extern uint32_t SDL_GetWindowID(SDL_Window *);
extern void SDL_DestroyWindow(SDL_Window *);
extern SDL_Window *SDL_GetWindowFromID(uint32_t);
extern uint32_t SDL_GetWindowProperties(SDL_Window *);
extern void *SDL_GetPointerProperty(uint32_t, const char *, void *);
extern void overink_macos_touch_event(uint32_t, uint64_t, uint32_t, float, float);

static NSWindow *OverInkCocoaWindow(SDL_Window *window) {
    if (window == NULL) {
        return nil;
    }
    return (__bridge NSWindow *)SDL_GetPointerProperty(
        SDL_GetWindowProperties(window), "SDL.window.cocoa.window", NULL);
}

void overink_macos_remove_metal_view(uint32_t window_id, intptr_t tag) {
    if ([NSThread isMainThread]) {
        [[OverInkCocoaWindow(SDL_GetWindowFromID(window_id)).contentView viewWithTag:tag]
            removeFromSuperview];
    }
}

// SDL invalidates each window's cursor rect when the picker or screenshot
// changes the cursor. External views supply the replacement rect themselves.
static BOOL OverInkOverlayCrosshair = NO;

void overink_macos_overlay_crosshair(bool crosshair) {
    if ([NSThread isMainThread]) {
        OverInkOverlayCrosshair = crosshair;
    }
}

// SDL attaches its input listener to external Cocoa windows and views. Supply
// the first-click and tracking behavior it normally installs on its own view.
@interface OverInkOverlayView : NSView
- (void)clearOverlayTracking;
@end

@implementation OverInkOverlayView {
    NSTrackingArea *_overlayTracking;
}

- (BOOL)acceptsFirstMouse:(NSEvent *)event {
    return YES;
}

- (void)rightMouseDown:(NSEvent *)event {
    [self.nextResponder rightMouseDown:event];
}

- (void)resetCursorRects {
    [super resetCursorRects];
    [self addCursorRect:self.bounds cursor:OverInkOverlayCrosshair
        ? NSCursor.crosshairCursor : NSCursor.arrowCursor];
}

- (void)clearOverlayTracking {
    if (_overlayTracking != nil) {
        [self removeTrackingArea:_overlayTracking];
        _overlayTracking = nil;
    }
}

- (void)updateTrackingAreas {
    [super updateTrackingAreas];
    [self clearOverlayTracking];
    if (self.nextResponder != nil) {
        _overlayTracking = [[NSTrackingArea alloc] initWithRect:NSZeroRect
            options:NSTrackingMouseEnteredAndExited | NSTrackingActiveAlways | NSTrackingInVisibleRect
            owner:self.nextResponder userInfo:nil];
        [self addTrackingArea:_overlayTracking];
    }
}
@end

@interface OverInkOverlayPanel : NSPanel
@property NSSize gripSize;
@end

@implementation OverInkOverlayPanel
- (BOOL)canBecomeKeyWindow {
    return YES;
}

- (BOOL)canBecomeMainWindow {
    return NO;
}

- (void)setStyleMask:(NSWindowStyleMask)mask {
    // SDL can update border/resizable bits after creating the native window.
    [super setStyleMask:mask | NSWindowStyleMaskNonactivatingPanel];
}

- (void)sendEvent:(NSEvent *)event {
    if (event.type == NSEventTypeLeftMouseDown && self.gripSize.height > 0
        && event.subtype != NSEventSubtypeTabletPoint
        && (event.modifierFlags & NSEventModifierFlagControl) == 0) {
        NSRect content = self.contentView.frame;
        NSRect grip = NSMakeRect(NSMinX(content), NSMaxY(content) - self.gripSize.height,
            self.gripSize.width, self.gripSize.height);
        if (NSPointInRect(event.locationInWindow, grip)) {
            [self performWindowDragWithEvent:event];
            return;
        }
    }
    [super sendEvent:event];
}
@end

uint32_t overink_macos_create_overlay(int32_t width, int32_t height, uint64_t flags,
                                    float grip_width, float grip_height) {
    if (![NSThread isMainThread] || width <= 0 || height <= 0) {
        return 0;
    }
    NSRect rect = NSMakeRect(0, 0, width, height);
    OverInkOverlayPanel *panel;
    NSWindowStyleMask style = NSWindowStyleMaskBorderless | NSWindowStyleMaskNonactivatingPanel;
    if (grip_height <= 0) {
        style |= NSWindowStyleMaskResizable;
    }
    @try {
        panel = [[OverInkOverlayPanel alloc]
            initWithContentRect:rect
            styleMask:style
            backing:NSBackingStoreBuffered defer:NO];
    } @catch (NSException *exception) {
        return 0;
    }
    if (panel == nil) {
        return 0;
    }
    panel.gripSize = NSMakeSize(grip_width, grip_height);
    panel.releasedWhenClosed = NO;
    panel.hidesOnDeactivate = NO;
    panel.colorSpace = NSColorSpace.sRGBColorSpace;
    panel.tabbingMode = NSWindowTabbingModeDisallowed;
    NSWindowCollectionBehavior behavior = NSWindowCollectionBehaviorCanJoinAllSpaces
        | NSWindowCollectionBehaviorFullScreenAuxiliary | NSWindowCollectionBehaviorIgnoresCycle;
    if (@available(macOS 13.0, *)) {
        // NSWindowCollectionBehaviorCanJoinAllApplications, including when
        // building with a macOS 11 SDK that predates Stage Manager.
        const NSWindowCollectionBehavior canJoinAllApplications = 1UL << 18;
        behavior |= canJoinAllApplications;
    }
    panel.collectionBehavior = behavior;
    [panel center];
    OverInkOverlayView *view = [[OverInkOverlayView alloc] initWithFrame:rect];
    panel.contentView = view;

    uint32_t props = SDL_CreateProperties();
    SDL_Window *window = NULL;
    if (props != 0) {
        if (SDL_SetPointerProperty(props, "SDL.window.create.cocoa.window", (__bridge void *)panel)
            && SDL_SetNumberProperty(props, "SDL.window.create.width", width)
            && SDL_SetNumberProperty(props, "SDL.window.create.height", height)
            && SDL_SetNumberProperty(props, "SDL.window.create.flags", (int64_t)flags)) {
            window = SDL_CreateWindowWithProperties(props);
        }
        SDL_DestroyProperties(props);
    }
    if (window == NULL) {
        [panel close];
        return 0;
    }
    uint32_t id = SDL_GetWindowID(window);
    if (id == 0) {
        [view clearOverlayTracking];
        SDL_DestroyWindow(window);
        [panel close];
        return 0;
    }
    [view updateTrackingAreas];
    return id;
}

bool overink_macos_raise_overlay(uint32_t window_id, bool keyboard) {
    if (![NSThread isMainThread]) {
        return false;
    }
    NSWindow *panel = OverInkCocoaWindow(SDL_GetWindowFromID(window_id));
    if (![panel isKindOfClass:OverInkOverlayPanel.class]) {
        return false;
    }
    if (panel.isVisible) {
        [panel orderFrontRegardless];
        if (keyboard && !panel.isKeyWindow) {
            [panel makeKeyWindow];
        }
    }
    return true;
}

void overink_macos_order_toolbar(uint32_t canvas_id, uint32_t toolbar_id) {
    if (![NSThread isMainThread]) {
        return;
    }
    NSWindow *canvas = OverInkCocoaWindow(SDL_GetWindowFromID(canvas_id));
    NSWindow *toolbar = OverInkCocoaWindow(SDL_GetWindowFromID(toolbar_id));
    if (canvas == nil || toolbar == nil || !toolbar.isVisible) {
        return;
    }
    NSInteger level = canvas.level;
    if (level < NSIntegerMax) {
        ++level;
    }
    if (toolbar.level != level) {
        toolbar.level = level;
    }
    [toolbar orderWindow:NSWindowAbove relativeTo:canvas.windowNumber];
}

bool overink_macos_destroy_overlay(uint32_t window_id) {
    if (![NSThread isMainThread]) {
        return false;
    }
    SDL_Window *window = SDL_GetWindowFromID(window_id);
    NSWindow *panel = OverInkCocoaWindow(window);
    if (![panel isKindOfClass:OverInkOverlayPanel.class]) {
        return false;
    }
    // Detach SDL's listener first. SDL does not close an externally supplied
    // window, so keep the panel alive here until AppKit closes it.
    NSView *view = panel.contentView;
    if ([view isKindOfClass:OverInkOverlayView.class]) {
        [(OverInkOverlayView *)view clearOverlayTracking];
    }
    SDL_DestroyWindow(window);
    [panel close];
    return true;
}

// Available starting with macOS 27; declare for older build SDKs while
// checking at runtime so macOS 11–26 continue to use their original path.
@interface NSTouch (OverInkDirectTouch27)
- (NSPoint)locationInView:(NSView *)view;
@end

typedef struct {
    uint32_t window_id;
    uint64_t finger_id;
    uint32_t phase;
    float x;
    float y;
} OverInkSample;

@interface OverInkDirectTouchRecognizer : NSGestureRecognizer
- (instancetype)initWithWindowID:(uint32_t)windowID;
@end

@implementation OverInkDirectTouchRecognizer {
    uint32_t _windowID;
    uint64_t _nextFinger;
    NSMutableDictionary<id<NSCopying>, NSNumber *> *_fingerIDs;
    NSMutableData *_pending;
    NSPoint _lastLocation;
}

- (instancetype)initWithWindowID:(uint32_t)windowID {
    self = [super initWithTarget:nil action:NULL];
    if (self != nil) {
        _windowID = windowID;
        _nextFinger = 0;
        _fingerIDs = [[NSMutableDictionary alloc] init];
        _pending = [[NSMutableData alloc] init];
        self.allowedTouchTypes = NSTouchTypeMaskDirect;
        self.target = self;
        self.action = @selector(publishTouches:);
    }
    return self;
}

- (void)collect:(NSEvent *)event phase:(NSTouchPhase)phase kind:(uint32_t)kind {
    NSView *view = self.view;
    if (view == nil) {
        return;
    }
    NSRect bounds = view.bounds;
    if (bounds.size.width <= 0 || bounds.size.height <= 0) {
        return;
    }
    for (NSTouch *touch in [event touchesMatchingPhase:phase inView:view]) {
        if (touch.type != NSTouchTypeDirect) {
            continue;
        }
        id<NSCopying> identity = touch.identity;
        if (identity == nil) {
            continue;
        }
        NSNumber *finger = _fingerIDs[identity];
        if (kind == 0) {
            if (finger != nil || _fingerIDs.count >= 16) {
                continue;
            }
            finger = @(++_nextFinger);
            _fingerIDs[identity] = finger;
        } else if (finger == nil) {
            continue;
        }
        if (![touch respondsToSelector:@selector(locationInView:)]) {
            continue;
        }
        // AppKit direct-touch coordinates are in view points, not SDL pixels.
        NSPoint point = [touch locationInView:view];
        if (!isfinite(point.x) || !isfinite(point.y)) {
            continue;
        }
        _lastLocation = point;
        const float x = (float)((point.x - bounds.origin.x) / bounds.size.width);
        const float y = (float)((view.isFlipped
            ? point.y - bounds.origin.y
            : NSMaxY(bounds) - point.y) / bounds.size.height);
        OverInkSample sample = {_windowID, finger.unsignedLongLongValue, kind, x, y};
        [_pending appendBytes:&sample length:sizeof(sample)];
        if (kind >= 2) {
            [_fingerIDs removeObjectForKey:identity];
        }
    }
}

- (void)publishTouches:(NSGestureRecognizer *)recognizer {
    (void)recognizer;
    // A gesture action runs on the AppKit main thread. The Rust callback
    // copies only scalars into SDL's event queue, never the NSTouch or NSView.
    const OverInkSample *samples = _pending.bytes;
    NSUInteger count = _pending.length / sizeof(OverInkSample);
    for (NSUInteger i = 0; i < count; ++i) {
        OverInkSample sample = samples[i];
        overink_macos_touch_event(sample.window_id, sample.finger_id,
                                sample.phase, sample.x, sample.y);
    }
    [_pending setLength:0];
}

- (void)advanceState {
    // End a gesture when its final finger is lifted; never monopolize
    // the view with a recognizer stuck in Began/Changed.
    if (_fingerIDs.count == 0) {
        self.state = NSGestureRecognizerStateEnded;
    } else if (self.state == NSGestureRecognizerStatePossible) {
        self.state = NSGestureRecognizerStateBegan;
    } else {
        self.state = NSGestureRecognizerStateChanged;
    }
    // A second motion can occur while already in Changed. Deliver every
    // queued sample even if AppKit doesn't emit a new state transition.
    if (_pending.length != 0) {
        [self publishTouches:self];
    }
}

- (void)touchesBeganWithEvent:(NSEvent *)event {
    [self collect:event phase:NSTouchPhaseBegan kind:0];
    if (_pending.length != 0) {
        [self advanceState];
    }
}

- (void)touchesMovedWithEvent:(NSEvent *)event {
    [self collect:event phase:NSTouchPhaseMoved kind:1];
    if (_pending.length != 0) {
        [self advanceState];
    }
}

- (void)touchesEndedWithEvent:(NSEvent *)event {
    [self collect:event phase:NSTouchPhaseEnded kind:2];
    if (_pending.length != 0) {
        [self advanceState];
    }
}

- (void)touchesCancelledWithEvent:(NSEvent *)event {
    [self collect:event phase:NSTouchPhaseCancelled kind:3];
    if (_pending.length != 0) {
        [self publishTouches:self];
    }
    self.state = NSGestureRecognizerStateCancelled;
}

- (NSPoint)locationInView:(NSView *)view {
    NSView *ownView = self.view;
    if (view == nil || ownView == nil || view == ownView) {
        return _lastLocation;
    }
    return [ownView convertPoint:_lastLocation toView:view];
}

- (void)reset {
    [super reset];
    [_fingerIDs removeAllObjects];
    [_pending setLength:0];
    _lastLocation = NSZeroPoint;
}
@end

static char OverInkDirectTouchKey;

int32_t overink_macos_install_touch(uint32_t window_id) {
    // This entry point is safe for any scalar ID, including off-main-thread
    // callers. Cocoa and SDL window access must remain on the AppKit thread.
    if (![NSThread isMainThread]) {
        return -1;
    }
    if (@available(macOS 27.0, *)) {
        SDL_Window *window = SDL_GetWindowFromID(window_id);
        if (window == NULL) {
            return -1;
        }
        NSWindow *nsWindow = OverInkCocoaWindow(window);
        NSView *view = nsWindow.contentView;
        if (view == nil) {
            return -1;
        }
        if (objc_getAssociatedObject(nsWindow, &OverInkDirectTouchKey) != nil) {
            return 1;
        }
        OverInkDirectTouchRecognizer *recognizer =
            [[OverInkDirectTouchRecognizer alloc] initWithWindowID:window_id];
        if (recognizer == nil) {
            return -1;
        }
        [view addGestureRecognizer:recognizer];
        objc_setAssociatedObject(nsWindow, &OverInkDirectTouchKey, recognizer,
                                 OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        return 1;
    }
    return 0; // macOS 11–26: preserve SDL's existing input behavior.
}

void overink_macos_uninstall_touch(uint32_t window_id) {
    if (![NSThread isMainThread]) {
        return;
    }
    SDL_Window *window = SDL_GetWindowFromID(window_id);
    if (window == NULL) {
        return;
    }
    NSWindow *nsWindow = (__bridge NSWindow *)SDL_GetPointerProperty(
        SDL_GetWindowProperties(window), "SDL.window.cocoa.window", NULL);
    if (nsWindow == nil) {
        return;
    }
    NSGestureRecognizer *recognizer =
        objc_getAssociatedObject(nsWindow, &OverInkDirectTouchKey);
    if (recognizer != nil) {
        [recognizer.view removeGestureRecognizer:recognizer];
        objc_setAssociatedObject(nsWindow, &OverInkDirectTouchKey, nil,
                                 OBJC_ASSOCIATION_ASSIGN);
    }
}
