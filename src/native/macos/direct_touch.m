// macOS 27 direct-touch bridge. This is the only new AppKit gesture boundary.
// Cocoa and SDL handles stay inside Objective-C; Rust receives scalar events.
#import <AppKit/AppKit.h>
#import <objc/runtime.h>
#include <stdint.h>
#include <math.h>

typedef struct SDL_Window SDL_Window;
extern SDL_Window *SDL_GetWindowFromID(uint32_t);
extern uint32_t SDL_GetWindowProperties(SDL_Window *);
extern void *SDL_GetPointerProperty(uint32_t, const char *, void *);
extern void overink_macos_touch_event(uint32_t, uint64_t, uint32_t, float, float);

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
    if (@available(macOS 27.0, *)) {
        SDL_Window *window = SDL_GetWindowFromID(window_id);
        if (window == NULL) {
            return -1;
        }
        NSWindow *nsWindow = (__bridge NSWindow *)SDL_GetPointerProperty(
            SDL_GetWindowProperties(window), "SDL.window.cocoa.window", NULL);
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
