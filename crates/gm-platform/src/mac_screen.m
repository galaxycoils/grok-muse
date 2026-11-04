#import <Foundation/Foundation.h>
#import <AppKit/AppKit.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreGraphics/CoreGraphics.h>
#import <ScreenCaptureKit/ScreenCaptureKit.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <ImageIO/ImageIO.h>
#import <EventKit/EventKit.h>
#import <UserNotifications/UserNotifications.h>
#import <Carbon/Carbon.h>
#include <os/lock.h>

// Stream frame storage
typedef struct {
    uint8_t *data;
    size_t len;
    uint32_t width;
    uint32_t height;
    double scale;
    uint64_t timestamp_ms;
} FrameBuffer;

static FrameBuffer g_latest_frame = { NULL, 0, 0, 0, 1.0, 0 };
static CMSampleBufferRef g_latest_sample = NULL;
static uint64_t g_latest_sample_ts_ms = 0;
static os_unfair_lock g_frame_lock = OS_UNFAIR_LOCK_INIT;
static SCStream *g_stream = nil;
static uint32_t g_stream_width = 1680;
static uint32_t g_stream_height = 1050;
static double g_cached_backing_scale = 0.0;
static int g_capture_active_state = -1;

static double gm_query_display_backing_scale(CGDirectDisplayID displayID) {
    CGDisplayModeRef mode = CGDisplayCopyDisplayMode(displayID);
    if (mode) {
        size_t pixelW = CGDisplayModeGetPixelWidth(mode);
        size_t pointW = CGDisplayModeGetWidth(mode);
        CGDisplayModeRelease(mode);
        if (pointW > 0 && pixelW >= pointW) {
            return (double)pixelW / (double)pointW;
        }
    }
    return 2.0;
}

double gm_mac_get_primary_backing_scale(void) {
    if (g_cached_backing_scale <= 0.0) {
        g_cached_backing_scale = gm_query_display_backing_scale(CGMainDisplayID());
    }
    return g_cached_backing_scale;
}

uint64_t gm_mac_peek_latest_frame_timestamp(void) {
    os_unfair_lock_lock(&g_frame_lock);
    uint64_t ts = g_latest_sample ? g_latest_sample_ts_ms : g_latest_frame.timestamp_ms;
    os_unfair_lock_unlock(&g_frame_lock);
    return ts;
}

@interface GmStreamOutputDelegate : NSObject <SCStreamOutput, SCStreamDelegate>
@end

@implementation GmStreamOutputDelegate
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer ofType:(SCStreamOutputType)type {
    (void)stream;
    if (type != SCStreamOutputTypeScreen || !sampleBuffer) return;

    CVPixelBufferRef pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pixelBuffer) return;

    // Lazy retention: keep only a CFRetain reference to the latest IOSurface-backed CMSampleBuffer.
    // Zero malloc/memcpy occurs on the real-time ScreenCaptureKit callback queue.
    uint64_t now_ms = (uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0);
    CMSampleBufferRef retained = (CMSampleBufferRef)CFRetain(sampleBuffer);
    CMSampleBufferRef previous = NULL;

    os_unfair_lock_lock(&g_frame_lock);
    previous = g_latest_sample;
    g_latest_sample = retained;
    g_latest_sample_ts_ms = now_ms;
    os_unfair_lock_unlock(&g_frame_lock);

    if (previous) {
        CFRelease(previous);
    }
}
@end

static GmStreamOutputDelegate *g_stream_delegate = nil;

int gm_mac_check_screen_recording_permission(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

int gm_mac_check_ax_permission(void) {
    return AXIsProcessTrusted() ? 1 : 0;
}

#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"

int gm_mac_check_calendar_permission(void) {
    EKAuthorizationStatus status = [EKEventStore authorizationStatusForEntityType:EKEntityTypeEvent];
    return (status >= 3) ? 1 : 0;
}

int gm_mac_check_reminders_permission(void) {
    EKAuthorizationStatus status = [EKEventStore authorizationStatusForEntityType:EKEntityTypeReminder];
    return (status >= 3) ? 1 : 0;
}

int gm_mac_request_screen_recording_permission(void) {
    return CGRequestScreenCaptureAccess() ? 1 : 0;
}

int gm_mac_request_ax_permission(void) {
    NSDictionary *options = @{(__bridge id)kAXTrustedCheckOptionPrompt: @YES};
    return AXIsProcessTrustedWithOptions((__bridge CFDictionaryRef)options) ? 1 : 0;
}

int gm_mac_request_calendar_permission(void) {
    @autoreleasepool {
        EKEventStore *store = [[EKEventStore alloc] init];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block int result = 0;
        if (@available(macOS 14.0, *)) {
            [store requestFullAccessToEventsWithCompletion:^(BOOL granted, NSError * _Nullable error) {
                (void)error;
                result = granted ? 1 : 0;
                dispatch_semaphore_signal(sem);
            }];
        } else {
            [store requestAccessToEntityType:EKEntityTypeEvent completion:^(BOOL granted, NSError * _Nullable error) {
                (void)error;
                result = granted ? 1 : 0;
                dispatch_semaphore_signal(sem);
            }];
        }
        dispatch_time_t timeout = dispatch_time(DISPATCH_TIME_NOW, (int64_t)(3 * NSEC_PER_SEC));
        dispatch_semaphore_wait(sem, timeout);
        return result;
    }
}

int gm_mac_request_reminders_permission(void) {
    @autoreleasepool {
        EKEventStore *store = [[EKEventStore alloc] init];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block int result = 0;
        if (@available(macOS 14.0, *)) {
            [store requestFullAccessToRemindersWithCompletion:^(BOOL granted, NSError * _Nullable error) {
                (void)error;
                result = granted ? 1 : 0;
                dispatch_semaphore_signal(sem);
            }];
        } else {
            [store requestAccessToEntityType:EKEntityTypeReminder completion:^(BOOL granted, NSError * _Nullable error) {
                (void)error;
                result = granted ? 1 : 0;
                dispatch_semaphore_signal(sem);
            }];
        }
        dispatch_time_t timeout = dispatch_time(DISPATCH_TIME_NOW, (int64_t)(3 * NSEC_PER_SEC));
        dispatch_semaphore_wait(sem, timeout);
        return result;
    }
}

#pragma clang diagnostic pop

int gm_mac_open_system_settings_pane(const char *pane_name) {
    @autoreleasepool {
        if (!pane_name) return -1;
        NSString *pane = [NSString stringWithUTF8String:pane_name];
        NSString *urlStr = [pane hasPrefix:@"x-apple.systempreferences:"]
            ? pane
            : [NSString stringWithFormat:@"x-apple.systempreferences:com.apple.preference.security?%@", pane];
        NSURL *url = [NSURL URLWithString:urlStr];
        if (!url) return -1;
        BOOL ok = [[NSWorkspace sharedWorkspace] openURL:url];
        return ok ? 0 : -1;
    }
}

// Global hotkeys (Carbon EventHotKey)
static EventHandlerRef g_carbon_hotkey_handler = NULL;
static void (*g_carbon_hotkey_callback)(int) = NULL;
static EventHotKeyRef g_summon_hotkey_ref = NULL;
static EventHotKeyRef g_kill_hotkey_ref = NULL;

static OSStatus GmCarbonHotkeyHandler(EventHandlerCallRef nextHandler, EventRef theEvent, void *userData) {
    (void)nextHandler;
    (void)userData;
    EventHotKeyID hotKeyID;
    OSStatus err = GetEventParameter(theEvent, kEventParamDirectObject, typeEventHotKeyID, NULL, sizeof(hotKeyID), NULL, &hotKeyID);
    if (err == noErr && g_carbon_hotkey_callback) {
        g_carbon_hotkey_callback((int)hotKeyID.id);
    }
    return noErr;
}

int gm_mac_install_global_hotkeys(void (*callback)(int hotkey_id)) {
    g_carbon_hotkey_callback = callback;
    if (g_carbon_hotkey_handler) {
        return 0; // Already installed
    }
    EventTypeSpec eventType;
    eventType.eventClass = kEventClassKeyboard;
    eventType.eventKind = kEventHotKeyPressed;
    OSStatus status = InstallApplicationEventHandler(&GmCarbonHotkeyHandler, 1, &eventType, NULL, &g_carbon_hotkey_handler);
    if (status != noErr) {
        return (int)status;
    }
    
    // Register Summon: Cmd + Shift + Space (Space keycode is 49 = 0x31)
    EventHotKeyID summonID = { 'GMUS', 1 };
    RegisterEventHotKey(49, cmdKey | shiftKey, summonID, GetApplicationEventTarget(), 0, &g_summon_hotkey_ref);
    
    // Register Kill Switch: Cmd + Option + Escape (Escape keycode is 53 = 0x35)
    EventHotKeyID killID = { 'GMUS', 2 };
    RegisterEventHotKey(53, cmdKey | optionKey, killID, GetApplicationEventTarget(), 0, &g_kill_hotkey_ref);
    
    return 0;
}

void gm_mac_uninstall_global_hotkeys(void) {
    if (g_summon_hotkey_ref) {
        UnregisterEventHotKey(g_summon_hotkey_ref);
        g_summon_hotkey_ref = NULL;
    }
    if (g_kill_hotkey_ref) {
        UnregisterEventHotKey(g_kill_hotkey_ref);
        g_kill_hotkey_ref = NULL;
    }
    if (g_carbon_hotkey_handler) {
        RemoveEventHandler(g_carbon_hotkey_handler);
        g_carbon_hotkey_handler = NULL;
    }
    g_carbon_hotkey_callback = NULL;
}

double gm_mac_seconds_since_last_input(void) {
    return CGEventSourceSecondsSinceLastEventType(kCGEventSourceStateCombinedSessionState, kCGAnyInputEventType);
}

int gm_mac_start_stream(void) {
    @autoreleasepool {
        if (g_stream) return 1; // Already running
        
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block int success = 0;
        
        [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *content, NSError *error) {
            if (error || !content || content.displays.count == 0) {
                dispatch_semaphore_signal(sem);
                return;
            }
            
            SCDisplay *display = nil;
            for (SCDisplay *candidate in content.displays) { if (candidate.displayID == CGMainDisplayID()) { display = candidate; break; } }
            if (!display) { dispatch_semaphore_signal(sem); return; }
            g_cached_backing_scale = gm_query_display_backing_scale(display.displayID);
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
            SCStreamConfiguration *config = [[SCStreamConfiguration alloc] init];
            // Capture at 1:1 macOS logical point coordinates so SoM badges align 1:1 with AXUIElement & CGEvent
            uint32_t pointW = (uint32_t)(display.width > 0 ? display.width : 1680);
            uint32_t pointH = (uint32_t)(display.height > 0 ? display.height : 1050);
            g_stream_width = pointW;
            g_stream_height = pointH;
            config.width = pointW;
            config.height = pointH;
            // Start in low-power 2 FPS standby; gm_mac_set_capture_active(1) boosts to 20 FPS during active tasks
            config.minimumFrameInterval = CMTimeMake(1, 2);
            config.pixelFormat = 'BGRA';
            config.showsCursor = YES;
            
            g_stream_delegate = [[GmStreamOutputDelegate alloc] init];
            g_stream = [[SCStream alloc] initWithFilter:filter configuration:config delegate:g_stream_delegate];
            
            dispatch_queue_t queue = dispatch_queue_create("co.anysphere.grok-muse.capture-queue", DISPATCH_QUEUE_SERIAL);
            NSError *addOutputError = nil;
            [g_stream addStreamOutput:g_stream_delegate type:SCStreamOutputTypeScreen sampleHandlerQueue:queue error:&addOutputError];
            
            if (addOutputError) {
                g_stream = nil;
                dispatch_semaphore_signal(sem);
                return;
            }
            
            [g_stream startCaptureWithCompletionHandler:^(NSError *startError) {
                if (!startError) {
                    success = 1;
                }
                dispatch_semaphore_signal(sem);
            }];
        }];
        
        dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
        return success;
    }
}

int gm_mac_set_capture_active(int active) {
    @autoreleasepool {
        if (!g_stream) return 0;
        int normalized = active ? 1 : 0;
        if (g_capture_active_state == normalized) return 0;
        g_capture_active_state = normalized;
        SCStreamConfiguration *config = [[SCStreamConfiguration alloc] init];
        config.width = g_stream_width > 0 ? g_stream_width : 1680;
        config.height = g_stream_height > 0 ? g_stream_height : 1050;
        config.minimumFrameInterval = normalized ? CMTimeMake(1, 20) : CMTimeMake(1, 1);
        config.pixelFormat = 'BGRA';
        config.showsCursor = YES;
        [g_stream updateConfiguration:config completionHandler:nil];
        return 0;
    }
}

static CGImageRef gm_create_cgimage_from_sample(CMSampleBufferRef sample) {
    if (!sample) return NULL;
    CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(sample);
    if (!pb) return NULL;

    CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    size_t width = CVPixelBufferGetWidth(pb);
    size_t height = CVPixelBufferGetHeight(pb);
    size_t bytesPerRow = CVPixelBufferGetBytesPerRow(pb);
    void *baseAddress = CVPixelBufferGetBaseAddress(pb);

    CGImageRef cgImg = NULL;
    if (baseAddress && width > 0 && height > 0) {
        CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
        CGContextRef ctx = CGBitmapContextCreate(
            baseAddress, width, height, 8, bytesPerRow, cs,
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little
        );
        CGColorSpaceRelease(cs);
        if (ctx) {
            cgImg = CGBitmapContextCreateImage(ctx);
            CGContextRelease(ctx);
        }
    }
    CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
    return cgImg;
}

int gm_mac_capture_jpeg_scaled(
    uint32_t max_long_edge,
    float quality,
    uint8_t **out_jpeg,
    size_t *out_len,
    uint32_t *out_w,
    uint32_t *out_h,
    double *out_scale,
    uint64_t *out_ts
) {
    if (!out_jpeg || !out_len || !out_w || !out_h || !out_scale || !out_ts) return -1;
    @autoreleasepool {
        double backingScale = gm_mac_get_primary_backing_scale();
        CMSampleBufferRef sample = NULL;
        uint64_t sample_ts = 0;

        os_unfair_lock_lock(&g_frame_lock);
        if (g_latest_sample) {
            sample = (CMSampleBufferRef)CFRetain(g_latest_sample);
            sample_ts = g_latest_sample_ts_ms;
        }
        os_unfair_lock_unlock(&g_frame_lock);

        CGImageRef sourceImage = NULL;
        if (sample) {
            sourceImage = gm_create_cgimage_from_sample(sample);
            CFRelease(sample);
        }

        if (!sourceImage) {
            dispatch_semaphore_t sem = dispatch_semaphore_create(0);
            __block CGImageRef shot = NULL;
            [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *content, NSError *error) {
                if (error || !content || content.displays.count == 0) {
                    dispatch_semaphore_signal(sem);
                    return;
                }
                SCDisplay *display = nil;
            for (SCDisplay *candidate in content.displays) { if (candidate.displayID == CGMainDisplayID()) { display = candidate; break; } }
            if (!display) { dispatch_semaphore_signal(sem); return; }
                SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
                SCStreamConfiguration *config = [[SCStreamConfiguration alloc] init];
                config.width = (size_t)((display.width > 0 ? display.width : 1680) * backingScale);
                config.height = (size_t)((display.height > 0 ? display.height : 1050) * backingScale);
                config.pixelFormat = 'BGRA';
                [SCScreenshotManager captureImageWithFilter:filter configuration:config completionHandler:^(CGImageRef img, NSError *err) {
                    if (img && !err) {
                        shot = CGImageRetain(img);
                    }
                    dispatch_semaphore_signal(sem);
                }];
            }];
            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
            sourceImage = shot;
            sample_ts = (uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0);
        }

        if (!sourceImage) return -2;

        size_t srcW = CGImageGetWidth(sourceImage);
        size_t srcH = CGImageGetHeight(sourceImage);
        size_t dstW = srcW;
        size_t dstH = srcH;
        size_t maxDim = srcW > srcH ? srcW : srcH;

        if (max_long_edge > 0 && maxDim > max_long_edge) {
            double ratio = (double)max_long_edge / (double)maxDim;
            dstW = (size_t)llround((double)srcW * ratio);
            dstH = (size_t)llround((double)srcH * ratio);
            if (dstW < 1) dstW = 1;
            if (dstH < 1) dstH = 1;
        }

        CGImageRef targetImage = NULL;
        if (dstW != srcW || dstH != srcH) {
            CGColorSpaceRef cs = CGColorSpaceCreateDeviceRGB();
            CGContextRef scaleCtx = CGBitmapContextCreate(
                NULL, dstW, dstH, 8, dstW * 4, cs,
                kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little
            );
            CGColorSpaceRelease(cs);
            if (scaleCtx) {
                CGContextSetInterpolationQuality(scaleCtx, kCGInterpolationHigh);
                CGContextDrawImage(scaleCtx, CGRectMake(0, 0, dstW, dstH), sourceImage);
                targetImage = CGBitmapContextCreateImage(scaleCtx);
                CGContextRelease(scaleCtx);
            }
            CGImageRelease(sourceImage);
        } else {
            targetImage = sourceImage;
        }

        if (!targetImage) return -3;

        NSMutableData *destData = [NSMutableData data];
        CGImageDestinationRef dest = CGImageDestinationCreateWithData(
            (__bridge CFMutableDataRef)destData, CFSTR("public.jpeg"), 1, NULL
        );
        if (!dest) {
            CGImageRelease(targetImage);
            return -4;
        }

        float q = (quality > 0.0f && quality <= 1.0f) ? quality : 0.75f;
        NSDictionary *options = @{
            (__bridge NSString *)kCGImageDestinationLossyCompressionQuality: @(q)
        };
        CGImageDestinationAddImage(dest, targetImage, (__bridge CFDictionaryRef)options);
        BOOL ok = CGImageDestinationFinalize(dest);
        CFRelease(dest);
        CGImageRelease(targetImage);

        if (!ok || [destData length] == 0) return -5;

        size_t len = [destData length];
        uint8_t *copy = (uint8_t *)malloc(len);
        if (!copy) return -6;
        memcpy(copy, [destData bytes], len);

        *out_jpeg = copy;
        *out_len = len;
        *out_w = (uint32_t)dstW;
        *out_h = (uint32_t)dstH;
        *out_scale = backingScale;
        *out_ts = sample_ts;
        return 0;
    }
}

int gm_mac_get_latest_frame(uint8_t **out_data, size_t *out_len, uint32_t *out_w, uint32_t *out_h, double *out_scale, uint64_t *out_ts) {
    double backingScale = gm_mac_get_primary_backing_scale();
    CMSampleBufferRef retainedSample = NULL;
    uint64_t sampleTs = 0;

    os_unfair_lock_lock(&g_frame_lock);
    if (g_latest_sample) {
        retainedSample = (CMSampleBufferRef)CFRetain(g_latest_sample);
        sampleTs = g_latest_sample_ts_ms;
    } else if (g_latest_frame.data && g_latest_frame.len > 0) {
        uint8_t *copy = (uint8_t *)malloc(g_latest_frame.len);
        if (copy) {
            memcpy(copy, g_latest_frame.data, g_latest_frame.len);
            *out_data = copy;
            *out_len = g_latest_frame.len;
            *out_w = g_latest_frame.width;
            *out_h = g_latest_frame.height;
            *out_scale = g_latest_frame.scale;
            *out_ts = g_latest_frame.timestamp_ms;
            os_unfair_lock_unlock(&g_frame_lock);
            return 0; // Success from cached fallback
        }
    }
    os_unfair_lock_unlock(&g_frame_lock);

    if (retainedSample) {
        CVPixelBufferRef pb = CMSampleBufferGetImageBuffer(retainedSample);
        if (pb) {
            CVPixelBufferLockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
            size_t width = CVPixelBufferGetWidth(pb);
            size_t height = CVPixelBufferGetHeight(pb);
            size_t srcBpr = CVPixelBufferGetBytesPerRow(pb);
            size_t dstBpr = width * 4;
            size_t totalBytes = dstBpr * height;
            const uint8_t *src = (const uint8_t *)CVPixelBufferGetBaseAddress(pb);
            uint8_t *copy = (src && totalBytes > 0) ? (uint8_t *)malloc(totalBytes) : NULL;
            if (copy) {
                for (size_t row = 0; row < height; row++) {
                    memcpy(copy + row * dstBpr, src + row * srcBpr, dstBpr);
                }
                CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
                CFRelease(retainedSample);
                *out_data = copy;
                *out_len = totalBytes;
                *out_w = (uint32_t)width;
                *out_h = (uint32_t)height;
                *out_scale = backingScale;
                *out_ts = sampleTs;
                return 0;
            }
            CVPixelBufferUnlockBaseAddress(pb, kCVPixelBufferLock_ReadOnly);
        }
        CFRelease(retainedSample);
    }
    
    // Fallback: capture on-demand via ScreenCaptureKit
    @autoreleasepool {
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block CGImageRef image = NULL;
        
        [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *content, NSError *error) {
            if (error || !content || content.displays.count == 0) {
                dispatch_semaphore_signal(sem);
                return;
            }
            SCDisplay *display = nil;
            for (SCDisplay *candidate in content.displays) { if (candidate.displayID == CGMainDisplayID()) { display = candidate; break; } }
            if (!display) { dispatch_semaphore_signal(sem); return; }
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
            SCStreamConfiguration *config = [[SCStreamConfiguration alloc] init];
            config.width = display.width > 0 ? display.width : 1680;
            config.height = display.height > 0 ? display.height : 1050;
            config.pixelFormat = 'BGRA';
            [SCScreenshotManager captureImageWithFilter:filter configuration:config completionHandler:^(CGImageRef img, NSError *err) {
                if (img && !err) {
                    image = CGImageRetain(img);
                }
                dispatch_semaphore_signal(sem);
            }];
        }];
        
        dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC));
        if (!image) return -1;
        
        size_t width = CGImageGetWidth(image);
        size_t height = CGImageGetHeight(image);
        size_t bytesPerRow = width * 4;
        size_t totalBytes = bytesPerRow * height;
        
        uint8_t *buffer = (uint8_t *)malloc(totalBytes);
        if (!buffer) {
            CGImageRelease(image);
            return -2;
        }
        
        CGColorSpaceRef colorSpace = CGColorSpaceCreateDeviceRGB();
        CGContextRef context = CGBitmapContextCreate(
            buffer, width, height, 8, bytesPerRow, colorSpace,
            kCGImageAlphaPremultipliedFirst | kCGBitmapByteOrder32Little
        );
        CGColorSpaceRelease(colorSpace);
        
        if (!context) {
            free(buffer);
            CGImageRelease(image);
            return -3;
        }
        
        CGContextDrawImage(context, CGRectMake(0, 0, width, height), image);
        CGContextRelease(context);
        CGImageRelease(image);
        
        // Cache this frame into g_latest_frame
        os_unfair_lock_lock(&g_frame_lock);
        if (!g_latest_frame.data) {
            uint8_t *cache_copy = (uint8_t *)malloc(totalBytes);
            if (cache_copy) {
                memcpy(cache_copy, buffer, totalBytes);
                g_latest_frame.data = cache_copy;
                g_latest_frame.len = totalBytes;
                g_latest_frame.width = (uint32_t)width;
                g_latest_frame.height = (uint32_t)height;
                g_latest_frame.scale = 2.0;
                g_latest_frame.timestamp_ms = (uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0);
            }
        }
        os_unfair_lock_unlock(&g_frame_lock);
        
        *out_data = buffer;
        *out_len = totalBytes;
        *out_w = (uint32_t)width;
        *out_h = (uint32_t)height;
        *out_scale = 2.0;
        *out_ts = (uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0);
        return 0;
    }
}

void gm_mac_free_frame(uint8_t *data) {
    if (data) {
        free(data);
    }
}

void gm_mac_stop_stream(void) {
    g_capture_active_state = -1;
    if (g_stream) {
        [g_stream stopCaptureWithCompletionHandler:nil];
        g_stream = nil;
        g_stream_delegate = nil;
    }
    CMSampleBufferRef sampleToRelease = NULL;
    os_unfair_lock_lock(&g_frame_lock);
    sampleToRelease = g_latest_sample;
    g_latest_sample = NULL;
    g_latest_sample_ts_ms = 0;
    if (g_latest_frame.data) {
        free(g_latest_frame.data);
        g_latest_frame.data = NULL;
        g_latest_frame.len = 0;
    }
    os_unfair_lock_unlock(&g_frame_lock);
    if (sampleToRelease) {
        CFRelease(sampleToRelease);
    }
}

// Hardware-accelerated native JPEG encoding
int gm_mac_encode_jpeg(const uint8_t *rgba_bytes, uint32_t width, uint32_t height, float quality, uint8_t **out_jpeg, size_t *out_len) {
    if (!rgba_bytes || width == 0 || height == 0 || !out_jpeg || !out_len) return -1;
    @autoreleasepool {
        CGColorSpaceRef colorSpace = CGColorSpaceCreateDeviceRGB();
        CGContextRef context = CGBitmapContextCreate(
            (void *)rgba_bytes, width, height, 8, width * 4, colorSpace,
            kCGImageAlphaPremultipliedLast | kCGBitmapByteOrder32Big
        );
        CGColorSpaceRelease(colorSpace);
        if (!context) return -2;
        
        CGImageRef image = CGBitmapContextCreateImage(context);
        CGContextRelease(context);
        if (!image) return -3;
        
        NSMutableData *destData = [NSMutableData data];
        CGImageDestinationRef dest = CGImageDestinationCreateWithData((__bridge CFMutableDataRef)destData, CFSTR("public.jpeg"), 1, NULL);
        if (!dest) {
            CGImageRelease(image);
            return -4;
        }
        
        float q = (quality > 0.0f && quality <= 1.0f) ? quality : 0.70f;
        NSDictionary *options = @{
            (__bridge NSString *)kCGImageDestinationLossyCompressionQuality: @(q)
        };
        CGImageDestinationAddImage(dest, image, (__bridge CFDictionaryRef)options);
        BOOL success = CGImageDestinationFinalize(dest);
        CFRelease(dest);
        CGImageRelease(image);
        
        if (!success || [destData length] == 0) return -5;
        
        size_t len = [destData length];
        uint8_t *copy = (uint8_t *)malloc(len);
        if (!copy) return -6;
        memcpy(copy, [destData bytes], len);
        
        *out_jpeg = copy;
        *out_len = len;
        return 0;
    }
}

// Token-to-AXUIElement cache for AX-first action dispatch
static os_unfair_lock g_ax_map_lock = OS_UNFAIR_LOCK_INIT;
static NSMutableDictionary *g_ax_elements_map = nil;

int gm_mac_ax_press(const char *token) {
    if (!token) return -1;
    @autoreleasepool {
        os_unfair_lock_lock(&g_ax_map_lock);
        if (!g_ax_elements_map) {
            os_unfair_lock_unlock(&g_ax_map_lock);
            return -2;
        }
        NSString *tok = [NSString stringWithUTF8String:token];
        id val = g_ax_elements_map[tok];
        os_unfair_lock_unlock(&g_ax_map_lock);

        if (!val) return -3;
        
        AXUIElementRef el = (__bridge AXUIElementRef)val;
        AXError err = AXUIElementPerformAction(el, kAXPressAction);
        return (err == kAXErrorSuccess) ? 0 : (int)err;
    }
}

// AX Window Inspection
static void traverse_element(AXUIElementRef element, NSMutableArray *elements_arr, int *token_counter, int depth) {
    if (depth > 6 || *token_counter >= 120) return;
    
    CFTypeRef role_val = NULL;
    AXUIElementCopyAttributeValue(element, kAXRoleAttribute, &role_val);
    NSString *role = (role_val && CFGetTypeID(role_val) == CFStringGetTypeID()) ? (__bridge NSString *)role_val : @"Unknown";
    
    CFTypeRef title_val = NULL;
    AXUIElementCopyAttributeValue(element, kAXTitleAttribute, &title_val);
    if (!title_val) {
        AXUIElementCopyAttributeValue(element, kAXDescriptionAttribute, &title_val);
    }
    NSString *title = nil;
    if (title_val) {
        if (CFGetTypeID(title_val) == CFStringGetTypeID()) {
            title = (__bridge NSString *)title_val;
        } else if ([(__bridge id)title_val isKindOfClass:[NSAttributedString class]]) {
            title = [(__bridge NSAttributedString *)title_val string];
        }
    }
    
    CFTypeRef val_ref = NULL;
    AXUIElementCopyAttributeValue(element, kAXValueAttribute, &val_ref);
    NSString *value = (val_ref && CFGetTypeID(val_ref) == CFStringGetTypeID()) ? (__bridge NSString *)val_ref : nil;
    
    CGPoint pos = CGPointZero;
    CGSize size = CGSizeZero;
    AXValueRef pos_val = NULL;
    if (AXUIElementCopyAttributeValue(element, kAXPositionAttribute, (CFTypeRef *)&pos_val) == kAXErrorSuccess && pos_val) {
        AXValueGetValue(pos_val, kAXValueCGPointType, &pos);
        CFRelease(pos_val);
    }
    AXValueRef size_val = NULL;
    if (AXUIElementCopyAttributeValue(element, kAXSizeAttribute, (CFTypeRef *)&size_val) == kAXErrorSuccess && size_val) {
        AXValueGetValue(size_val, kAXValueCGSizeType, &size);
        CFRelease(size_val);
    }
    
    Boolean enabled = true;
    CFTypeRef enabled_val = NULL;
    if (AXUIElementCopyAttributeValue(element, kAXEnabledAttribute, &enabled_val) == kAXErrorSuccess && enabled_val) {
        if (CFGetTypeID(enabled_val) == CFBooleanGetTypeID()) {
            enabled = CFBooleanGetValue((CFBooleanRef)enabled_val);
        }
        CFRelease(enabled_val);
    }
    
    BOOL is_secure = [role isEqualToString:@"AXSecureTextField"];
    NSString *token = [NSString stringWithFormat:@"el_%d", (*token_counter)++];
    
    // Store in token element map
    os_unfair_lock_lock(&g_ax_map_lock);
    if (g_ax_elements_map) {
        g_ax_elements_map[token] = (__bridge id)element;
    }
    os_unfair_lock_unlock(&g_ax_map_lock);
    
    NSMutableDictionary *el_dict = [NSMutableDictionary dictionary];
    el_dict[@"token"] = token;
    el_dict[@"role"] = role;
    if (title) el_dict[@"label"] = title;
    if (value && !is_secure) el_dict[@"value"] = value;
    el_dict[@"frame"] = @{
        @"x": @(pos.x),
        @"y": @(pos.y),
        @"width": @(size.width),
        @"height": @(size.height)
    };
    el_dict[@"is_enabled"] = enabled ? @YES : @NO;
    el_dict[@"is_secure"] = is_secure ? @YES : @NO;
    
    [elements_arr addObject:el_dict];
    
    if (title_val) CFRelease(title_val);
    if (val_ref) CFRelease(val_ref);
    if (role_val) CFRelease(role_val);
    
    CFTypeRef children_val = NULL;
    if (AXUIElementCopyAttributeValue(element, kAXChildrenAttribute, &children_val) == kAXErrorSuccess && children_val) {
        if (CFGetTypeID(children_val) == CFArrayGetTypeID()) {
            CFArrayRef children = (CFArrayRef)children_val;
            CFIndex count = CFArrayGetCount(children);
            for (CFIndex i = 0; i < count; i++) {
                AXUIElementRef child = (AXUIElementRef)CFArrayGetValueAtIndex(children, i);
                traverse_element(child, elements_arr, token_counter, depth + 1);
            }
        }
        CFRelease(children_val);
    }
}

int gm_mac_inspect_front_window(uint32_t target_window_id, char **out_json_str) {
    @autoreleasepool {
        os_unfair_lock_lock(&g_ax_map_lock);
        if (!g_ax_elements_map) {
            g_ax_elements_map = [[NSMutableDictionary alloc] init];
        } else {
            [g_ax_elements_map removeAllObjects];
        }
        os_unfair_lock_unlock(&g_ax_map_lock);
        
        pid_t pid = 0;
        NSString *appName = nil;
        NSString *win_title = nil;
        CGPoint win_pos = CGPointZero;
        CGSize win_size = CGSizeMake(1440, 900);
        
        // If a target window ID was provided, query its owning PID and bounds
        if (target_window_id > 0) {
            CGWindowID winId = (CGWindowID)target_window_id;
            CFArrayRef list = CGWindowListCopyWindowInfo(kCGWindowListOptionIncludingWindow, winId);
            if (list && CFArrayGetCount(list) > 0) {
                NSDictionary *info = (__bridge NSDictionary *)CFArrayGetValueAtIndex(list, 0);
                NSNumber *ownerPid = info[(id)kCGWindowOwnerPID];
                if (ownerPid) pid = [ownerPid intValue];
                appName = info[(id)kCGWindowOwnerName];
                win_title = info[(id)kCGWindowName];
                
                NSDictionary *boundsDict = info[(id)kCGWindowBounds];
                if (boundsDict) {
                    CGRect rect;
                    if (CGRectMakeWithDictionaryRepresentation((__bridge CFDictionaryRef)boundsDict, &rect)) {
                        win_pos = rect.origin;
                        win_size = rect.size;
                    }
                }
            }
            if (list) CFRelease(list);
        }
        
        // Fallback to frontmost application if no target PID resolved
        if (pid == 0) {
            NSRunningApplication *frontApp = [[NSWorkspace sharedWorkspace] frontmostApplication];
            pid = frontApp ? frontApp.processIdentifier : 0;
            if (!appName) {
                appName = frontApp ? frontApp.localizedName : @"Finder";
            }
        }
        
        AXUIElementRef appRef = (pid > 0) ? AXUIElementCreateApplication(pid) : NULL;
        CFTypeRef windowRef = NULL;
        
        if (appRef) {
            AXUIElementCopyAttributeValue(appRef, kAXFocusedWindowAttribute, &windowRef);
            if (!windowRef) {
                AXUIElementCopyAttributeValue(appRef, kAXMainWindowAttribute, &windowRef);
            }
            if (!windowRef) {
                CFTypeRef windowsArr = NULL;
                if (AXUIElementCopyAttributeValue(appRef, kAXWindowsAttribute, &windowsArr) == kAXErrorSuccess && windowsArr) {
                    if (CFGetTypeID(windowsArr) == CFArrayGetTypeID() && CFArrayGetCount((CFArrayRef)windowsArr) > 0) {
                        windowRef = CFRetain(CFArrayGetValueAtIndex((CFArrayRef)windowsArr, 0));
                    }
                    CFRelease(windowsArr);
                }
            }
        }
        
        NSMutableArray *elements = [NSMutableArray array];
        int token_counter = 0;
        
        if (windowRef) {
            CFTypeRef title_val = NULL;
            if (AXUIElementCopyAttributeValue((AXUIElementRef)windowRef, kAXTitleAttribute, &title_val) == kAXErrorSuccess && title_val) {
                if (CFGetTypeID(title_val) == CFStringGetTypeID()) {
                    win_title = (__bridge NSString *)title_val;
                }
                CFRelease(title_val);
            }
            AXValueRef pos_val = NULL;
            if (AXUIElementCopyAttributeValue((AXUIElementRef)windowRef, kAXPositionAttribute, (CFTypeRef *)&pos_val) == kAXErrorSuccess && pos_val) {
                AXValueGetValue(pos_val, kAXValueCGPointType, &win_pos);
                CFRelease(pos_val);
            }
            AXValueRef size_val = NULL;
            if (AXUIElementCopyAttributeValue((AXUIElementRef)windowRef, kAXSizeAttribute, (CFTypeRef *)&size_val) == kAXErrorSuccess && size_val) {
                AXValueGetValue(size_val, kAXValueCGSizeType, &win_size);
                CFRelease(size_val);
            }
            
            traverse_element((AXUIElementRef)windowRef, elements, &token_counter, 0);
            CFRelease(windowRef);
        }
        if (appRef) CFRelease(appRef);
        
        NSDictionary *snapshot_dict = @{
            @"window_id": @(target_window_id > 0 ? target_window_id : 1),
            @"app_name": appName ?: @"Unknown",
            @"window_title": win_title ?: (appName ?: @""),
            @"bounds": @{
                @"x": @(win_pos.x),
                @"y": @(win_pos.y),
                @"width": @(win_size.width),
                @"height": @(win_size.height)
            },
            @"elements": elements,
            @"timestamp_ms": @((uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0))
        };
        
        NSData *json = [NSJSONSerialization dataWithJSONObject:snapshot_dict options:0 error:nil];
        if (!json) return -1;
        
        NSString *jsonStr = [[NSString alloc] initWithData:json encoding:NSUTF8StringEncoding];
        if (!jsonStr) return -2;
        
        const char *utf8 = jsonStr.UTF8String;
        size_t len = strlen(utf8);
        char *copy = (char *)malloc(len + 1);
        if (!copy) return -3;
        memcpy(copy, utf8, len + 1);
        *out_json_str = copy;
        return 0;
    }
}

void gm_mac_free_string(char *str) {
    if (str) {
        free(str);
    }
}

// Action Dispatch via CGEvent session taps
int gm_mac_post_mouse_click(double x, double y, int button_code, int click_count) {
    CGPoint pt = CGPointMake(x, y);
    CGMouseButton btn = (button_code == 1) ? kCGMouseButtonRight : ((button_code == 2) ? kCGMouseButtonCenter : kCGMouseButtonLeft);
    
    // If click_count is 0, perform a mouse move without button press
    if (click_count <= 0) {
        CGEventRef move = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, pt, btn);
        if (!move) return -1;
        CGEventPost(kCGSessionEventTap, move);
        CFRelease(move);
        return 0;
    }
    
    CGEventType downType = (button_code == 1) ? kCGEventRightMouseDown : ((button_code == 2) ? kCGEventOtherMouseDown : kCGEventLeftMouseDown);
    CGEventType upType = (button_code == 1) ? kCGEventRightMouseUp : ((button_code == 2) ? kCGEventOtherMouseUp : kCGEventLeftMouseUp);
    
    CGEventRef down = CGEventCreateMouseEvent(NULL, downType, pt, btn);
    if (!down) return -1;
    if (click_count > 1) {
        CGEventSetIntegerValueField(down, kCGMouseEventClickState, click_count);
    }
    CGEventPost(kCGSessionEventTap, down);
    CFRelease(down);
    
    CGEventRef up = CGEventCreateMouseEvent(NULL, upType, pt, btn);
    if (!up) return -2;
    if (click_count > 1) {
        CGEventSetIntegerValueField(up, kCGMouseEventClickState, click_count);
    }
    CGEventPost(kCGSessionEventTap, up);
    CFRelease(up);
    
    return 0;
}

int gm_mac_post_drag(double start_x, double start_y, double end_x, double end_y) {
    CGPoint start_pt = CGPointMake(start_x, start_y);
    CGPoint end_pt = CGPointMake(end_x, end_y);
    
    // Move to start
    CGEventRef move = CGEventCreateMouseEvent(NULL, kCGEventMouseMoved, start_pt, kCGMouseButtonLeft);
    if (move) {
        CGEventPost(kCGSessionEventTap, move);
        CFRelease(move);
    }
    
    // Mouse down at start
    CGEventRef down = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseDown, start_pt, kCGMouseButtonLeft);
    if (!down) return -1;
    CGEventPost(kCGSessionEventTap, down);
    CFRelease(down);
    
    // Drag to end
    CGEventRef drag = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseDragged, end_pt, kCGMouseButtonLeft);
    if (!drag) return -2;
    CGEventPost(kCGSessionEventTap, drag);
    CFRelease(drag);
    
    // Mouse up at end
    CGEventRef up = CGEventCreateMouseEvent(NULL, kCGEventLeftMouseUp, end_pt, kCGMouseButtonLeft);
    if (!up) return -3;
    CGEventPost(kCGSessionEventTap, up);
    CFRelease(up);
    
    return 0;
}

int gm_mac_post_type_text(const char *utf8_text) {
    if (!utf8_text) return -1;
    NSString *str = [NSString stringWithUTF8String:utf8_text];
    if (!str) return -2;
    
    NSUInteger len = [str length];
    for (NSUInteger i = 0; i < len; i++) {
        UniChar c = [str characterAtIndex:i];
        CGEventRef down = CGEventCreateKeyboardEvent(NULL, 0, true);
        CGEventKeyboardSetUnicodeString(down, 1, &c);
        CGEventPost(kCGSessionEventTap, down);
        CFRelease(down);
        
        CGEventRef up = CGEventCreateKeyboardEvent(NULL, 0, false);
        CGEventKeyboardSetUnicodeString(up, 1, &c);
        CGEventPost(kCGSessionEventTap, up);
        CFRelease(up);
    }
    return 0;
}

static CGKeyCode key_code_from_string(NSString *key) {
    NSString *lower = [key lowercaseString];
    if ([lower isEqualToString:@"return"] || [lower isEqualToString:@"enter"]) return 36;
    if ([lower isEqualToString:@"tab"]) return 48;
    if ([lower isEqualToString:@"space"]) return 49;
    if ([lower isEqualToString:@"delete"] || [lower isEqualToString:@"backspace"]) return 51;
    if ([lower isEqualToString:@"escape"] || [lower isEqualToString:@"esc"]) return 53;
    if ([lower isEqualToString:@"command"] || [lower isEqualToString:@"cmd"]) return 55;
    if ([lower isEqualToString:@"shift"]) return 56;
    if ([lower isEqualToString:@"option"] || [lower isEqualToString:@"alt"]) return 58;
    if ([lower isEqualToString:@"control"] || [lower isEqualToString:@"ctrl"]) return 59;
    if ([lower isEqualToString:@"left"]) return 123;
    if ([lower isEqualToString:@"right"]) return 124;
    if ([lower isEqualToString:@"down"]) return 125;
    if ([lower isEqualToString:@"up"]) return 126;
    
    // Letters
    if ([lower isEqualToString:@"a"]) return 0;
    if ([lower isEqualToString:@"s"]) return 1;
    if ([lower isEqualToString:@"d"]) return 2;
    if ([lower isEqualToString:@"f"]) return 3;
    if ([lower isEqualToString:@"h"]) return 4;
    if ([lower isEqualToString:@"g"]) return 5;
    if ([lower isEqualToString:@"z"]) return 6;
    if ([lower isEqualToString:@"x"]) return 7;
    if ([lower isEqualToString:@"c"]) return 8;
    if ([lower isEqualToString:@"v"]) return 9;
    if ([lower isEqualToString:@"b"]) return 11;
    if ([lower isEqualToString:@"q"]) return 12;
    if ([lower isEqualToString:@"w"]) return 13;
    if ([lower isEqualToString:@"e"]) return 14;
    if ([lower isEqualToString:@"r"]) return 15;
    if ([lower isEqualToString:@"y"]) return 16;
    if ([lower isEqualToString:@"t"]) return 17;
    if ([lower isEqualToString:@"1"]) return 18;
    if ([lower isEqualToString:@"2"]) return 19;
    if ([lower isEqualToString:@"3"]) return 20;
    if ([lower isEqualToString:@"4"]) return 21;
    if ([lower isEqualToString:@"6"]) return 22;
    if ([lower isEqualToString:@"5"]) return 23;
    if ([lower isEqualToString:@"="]) return 24;
    if ([lower isEqualToString:@"9"]) return 25;
    if ([lower isEqualToString:@"7"]) return 26;
    if ([lower isEqualToString:@"-"]) return 27;
    if ([lower isEqualToString:@"8"]) return 28;
    if ([lower isEqualToString:@"0"]) return 29;
    if ([lower isEqualToString:@"]"]) return 30;
    if ([lower isEqualToString:@"o"]) return 31;
    if ([lower isEqualToString:@"u"]) return 32;
    if ([lower isEqualToString:@"["]) return 33;
    if ([lower isEqualToString:@"i"]) return 34;
    if ([lower isEqualToString:@"p"]) return 35;
    if ([lower isEqualToString:@"l"]) return 37;
    if ([lower isEqualToString:@"j"]) return 38;
    if ([lower isEqualToString:@"'"]) return 39;
    if ([lower isEqualToString:@"k"]) return 40;
    if ([lower isEqualToString:@";"]) return 41;
    if ([lower isEqualToString:@"\\"]) return 42;
    if ([lower isEqualToString:@","]) return 43;
    if ([lower isEqualToString:@"/"]) return 44;
    if ([lower isEqualToString:@"n"]) return 45;
    if ([lower isEqualToString:@"m"]) return 46;
    if ([lower isEqualToString:@"."]) return 47;
    
    return 0;
}

int gm_mac_post_key_combo(const char **modifiers, int mod_count, const char *key) {
    if (!key) return -1;
    @autoreleasepool {
        CGEventFlags flags = 0;
        for (int i = 0; i < mod_count; i++) {
            if (!modifiers[i]) continue;
            NSString *m = [[NSString stringWithUTF8String:modifiers[i]] lowercaseString];
            if ([m isEqualToString:@"command"] || [m isEqualToString:@"cmd"] || [m isEqualToString:@"meta"]) {
                flags |= kCGEventFlagMaskCommand;
            } else if ([m isEqualToString:@"shift"]) {
                flags |= kCGEventFlagMaskShift;
            } else if ([m isEqualToString:@"option"] || [m isEqualToString:@"alt"]) {
                flags |= kCGEventFlagMaskAlternate;
            } else if ([m isEqualToString:@"control"] || [m isEqualToString:@"ctrl"]) {
                flags |= kCGEventFlagMaskControl;
            }
        }
        
        NSString *keyStr = [NSString stringWithUTF8String:key];
        CGKeyCode code = key_code_from_string(keyStr);
        
        CGEventRef down = CGEventCreateKeyboardEvent(NULL, code, true);
        if (!down) return -2;
        if (flags != 0) {
            CGEventSetFlags(down, flags);
        }
        if (code == 0 && [keyStr length] > 0) {
            UniChar c = [keyStr characterAtIndex:0];
            CGEventKeyboardSetUnicodeString(down, 1, &c);
        }
        CGEventPost(kCGSessionEventTap, down);
        CFRelease(down);
        
        CGEventRef up = CGEventCreateKeyboardEvent(NULL, code, false);
        if (!up) return -3;
        if (flags != 0) {
            CGEventSetFlags(up, flags);
        }
        if (code == 0 && [keyStr length] > 0) {
            UniChar c = [keyStr characterAtIndex:0];
            CGEventKeyboardSetUnicodeString(up, 1, &c);
        }
        CGEventPost(kCGSessionEventTap, up);
        CFRelease(up);
        
        return 0;
    }
}

int gm_mac_post_scroll(int32_t dx, int32_t dy) {
    CGEventRef scroll = CGEventCreateScrollWheelEvent2(NULL, kCGScrollEventUnitPixel, 2, dy, dx, 0);
    if (!scroll) return -1;
    CGEventPost(kCGSessionEventTap, scroll);
    CFRelease(scroll);
    return 0;
}

int gm_mac_focus_app(const char *app_name) {
    @autoreleasepool {
        if (!app_name) return -1;
        NSString *name = [NSString stringWithUTF8String:app_name];
        for (NSRunningApplication *app in [[NSWorkspace sharedWorkspace] runningApplications]) {
            if ([app.localizedName caseInsensitiveCompare:name] == NSOrderedSame) {
                [app activateWithOptions:0];
                return 0;
            }
        }
        return -2;
    }
}

int gm_mac_launch_app(const char *bundle_id) {
    @autoreleasepool {
        if (!bundle_id) return -1;
        NSString *bid = [NSString stringWithUTF8String:bundle_id];
        NSWorkspaceOpenConfiguration *config = [NSWorkspaceOpenConfiguration configuration];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block int res = -2;
        
        NSURL *appUrl = [[NSWorkspace sharedWorkspace] URLForApplicationWithBundleIdentifier:bid];
        if (!appUrl) return -3;
        
        [[NSWorkspace sharedWorkspace] openApplicationAtURL:appUrl
                                              configuration:config
                                          completionHandler:^(NSRunningApplication *app, NSError *error) {
            if (app && !error) {
                res = 0;
            }
            dispatch_semaphore_signal(sem);
        }];
        dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 1 * NSEC_PER_SEC));
        return res;
    }
}

int gm_mac_get_calendar_events(uint64_t start_ms, uint64_t end_ms, char **out_json_str) {
    @autoreleasepool {
        if (!out_json_str) return -1;
        *out_json_str = NULL;
        
        EKEventStore *store = [[EKEventStore alloc] init];
        NSDate *startDate = [NSDate dateWithTimeIntervalSince1970:(NSTimeInterval)start_ms / 1000.0];
        NSDate *endDate = [NSDate dateWithTimeIntervalSince1970:(NSTimeInterval)end_ms / 1000.0];
        
        NSPredicate *predicate = [store predicateForEventsWithStartDate:startDate endDate:endDate calendars:nil];
        NSArray<EKEvent *> *events = nil;
        @try {
            events = [store eventsMatchingPredicate:predicate];
        } @catch (NSException *ex) {
            events = @[];
        }
        
        NSMutableArray *results = [NSMutableArray array];
        if (events) {
            for (EKEvent *event in events) {
                NSMutableDictionary *dict = [NSMutableDictionary dictionary];
                dict[@"id"] = event.eventIdentifier ?: [[NSUUID UUID] UUIDString];
                dict[@"title"] = event.title ?: @"";
                dict[@"start_time_ms"] = @((uint64_t)([event.startDate timeIntervalSince1970] * 1000.0));
                dict[@"end_time_ms"] = @((uint64_t)([event.endDate timeIntervalSince1970] * 1000.0));
                if (event.location) dict[@"location"] = event.location;
                if (event.notes) dict[@"notes"] = event.notes;
                dict[@"is_all_day"] = @(event.isAllDay);
                [results addObject:dict];
            }
        }
        
        NSError *err = nil;
        NSData *jsonData = [NSJSONSerialization dataWithJSONObject:results options:0 error:&err];
        if (!jsonData || err) {
            *out_json_str = strdup("[]");
            return 0;
        }
        NSString *jsonStr = [[NSString alloc] initWithData:jsonData encoding:NSUTF8StringEncoding];
        *out_json_str = strdup([jsonStr UTF8String]);
        return 0;
    }
}

int gm_mac_get_reminders(int include_completed, char **out_json_str) {
    @autoreleasepool {
        if (!out_json_str) return -1;
        *out_json_str = NULL;
        
        EKEventStore *store = [[EKEventStore alloc] init];
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block NSMutableArray *results = [NSMutableArray array];
        
        NSPredicate *predicate = nil;
        if (!include_completed) {
            predicate = [store predicateForIncompleteRemindersWithDueDateStarting:nil ending:nil calendars:nil];
        } else {
            predicate = [store predicateForRemindersInCalendars:nil];
        }
        
        @try {
            [store fetchRemindersMatchingPredicate:predicate completion:^(NSArray<EKReminder *> *reminders) {
                if (reminders) {
                    for (EKReminder *rem in reminders) {
                        NSMutableDictionary *dict = [NSMutableDictionary dictionary];
                        dict[@"id"] = rem.calendarItemIdentifier ?: [[NSUUID UUID] UUIDString];
                        dict[@"title"] = rem.title ?: @"";
                        dict[@"is_completed"] = @(rem.isCompleted);
                        dict[@"priority"] = @(rem.priority);
                        if (rem.dueDateComponents) {
                            NSDate *dueDate = [[NSCalendar currentCalendar] dateFromComponents:rem.dueDateComponents];
                            if (dueDate) {
                                dict[@"due_date_ms"] = @((uint64_t)([dueDate timeIntervalSince1970] * 1000.0));
                            }
                        }
                        if (rem.calendar) {
                            dict[@"list_name"] = rem.calendar.title ?: @"";
                        }
                        [results addObject:dict];
                    }
                }
                dispatch_semaphore_signal(sem);
            }];
            dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 500 * NSEC_PER_MSEC));
        } @catch (NSException *ex) {
            // Catch if permission not granted
        }
        
        NSError *err = nil;
        NSData *jsonData = [NSJSONSerialization dataWithJSONObject:results options:0 error:&err];
        if (!jsonData || err) {
            *out_json_str = strdup("[]");
            return 0;
        }
        NSString *jsonStr = [[NSString alloc] initWithData:jsonData encoding:NSUTF8StringEncoding];
        *out_json_str = strdup([jsonStr UTF8String]);
        return 0;
    }
}

int gm_mac_menu_click(const char **path_items, int path_len) {
    if (!path_items || path_len == 0) return -1;
    @autoreleasepool {
        NSRunningApplication *frontApp = [[NSWorkspace sharedWorkspace] frontmostApplication];
        if (!frontApp) return -2;

        AXUIElementRef appRef = AXUIElementCreateApplication(frontApp.processIdentifier);
        if (!appRef) return -3;

        CFTypeRef menuBarRef = NULL;
        if (AXUIElementCopyAttributeValue(appRef, kAXMenuBarAttribute, &menuBarRef) != kAXErrorSuccess || !menuBarRef) {
            CFRelease(appRef);
            return -4;
        }

        AXUIElementRef currentElement = (AXUIElementRef)menuBarRef;
        for (int i = 0; i < path_len; i++) {
            NSString *target = [NSString stringWithUTF8String:path_items[i]];
            CFTypeRef childrenRef = NULL;
            if (AXUIElementCopyAttributeValue(currentElement, kAXChildrenAttribute, &childrenRef) != kAXErrorSuccess || !childrenRef) {
                CFRelease(currentElement);
                CFRelease(appRef);
                return -5;
            }

            AXUIElementRef foundChild = NULL;
            CFArrayRef children = (CFArrayRef)childrenRef;
            CFIndex count = CFArrayGetCount(children);
            for (CFIndex j = 0; j < count; j++) {
                AXUIElementRef child = (AXUIElementRef)CFArrayGetValueAtIndex(children, j);
                CFTypeRef titleRef = NULL;
                if (AXUIElementCopyAttributeValue(child, kAXTitleAttribute, &titleRef) == kAXErrorSuccess && titleRef) {
                    if (CFGetTypeID(titleRef) == CFStringGetTypeID()) {
                        NSString *title = (__bridge NSString *)titleRef;
                        if ([title caseInsensitiveCompare:target] == NSOrderedSame) {
                            foundChild = (AXUIElementRef)CFRetain(child);
                            CFRelease(titleRef);
                            break;
                        }
                    }
                    CFRelease(titleRef);
                }
            }
            CFRelease(childrenRef);

            if (!foundChild) {
                CFRelease(currentElement);
                CFRelease(appRef);
                return -6;
            }

            if (i == path_len - 1) {
                AXUIElementPerformAction(foundChild, kAXPressAction);
                CFRelease(foundChild);
                CFRelease(currentElement);
                CFRelease(appRef);
                return 0;
            } else {
                AXUIElementPerformAction(foundChild, kAXPressAction);
                CFRelease(currentElement);
                currentElement = foundChild;
            }
        }
        CFRelease(currentElement);
        CFRelease(appRef);
        return 0;
    }
}

int gm_mac_send_notification(const char *title, const char *subtitle, const char *body) {
    @autoreleasepool {
        NSString *nsTitle = title ? [NSString stringWithUTF8String:title] : @"Grok-Muse";
        NSString *nsSub = subtitle ? [NSString stringWithUTF8String:subtitle] : @"";
        NSString *nsBody = body ? [NSString stringWithUTF8String:body] : @"";

        // 1. Try native UserNotifications framework if in an app bundle
        if ([NSBundle mainBundle].bundleIdentifier != nil) {
            @try {
                UNUserNotificationCenter *center = [UNUserNotificationCenter currentNotificationCenter];
                UNMutableNotificationContent *content = [[UNMutableNotificationContent alloc] init];
                content.title = nsTitle;
                if (nsSub.length > 0) content.subtitle = nsSub;
                content.body = nsBody;
                content.sound = [UNNotificationSound defaultSound];

                UNTimeIntervalNotificationTrigger *trigger = [UNTimeIntervalNotificationTrigger triggerWithTimeInterval:0.1 repeats:NO];
                NSString *reqId = [[NSUUID UUID] UUIDString];
                UNNotificationRequest *request = [UNNotificationRequest requestWithIdentifier:reqId content:content trigger:trigger];

                dispatch_semaphore_t sem = dispatch_semaphore_create(0);
                __block BOOL delivered = NO;
                [center requestAuthorizationWithOptions:(UNAuthorizationOptionAlert | UNAuthorizationOptionSound) completionHandler:^(BOOL granted, NSError * _Nullable authErr) {
                    (void)authErr;
                    if (granted) {
                        [center addNotificationRequest:request withCompletionHandler:^(NSError * _Nullable reqErr) {
                            if (!reqErr) delivered = YES;
                            dispatch_semaphore_signal(sem);
                        }];
                    } else {
                        dispatch_semaphore_signal(sem);
                    }
                }];
                dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 500 * NSEC_PER_MSEC));
                if (delivered) return 0;
            } @catch (NSException *ex) {
                // Fall through to AppleScript
            }
        }

        // 2. Fallback to NSAppleScript with robust string escaping
        NSString *escBody = [[[nsBody stringByReplacingOccurrencesOfString:@"\\" withString:@"\\\\"]
                                     stringByReplacingOccurrencesOfString:@"\"" withString:@"\\\""]
                                     stringByReplacingOccurrencesOfString:@"\n" withString:@"\\n"];
        NSString *escTitle = [[[nsTitle stringByReplacingOccurrencesOfString:@"\\" withString:@"\\\\"]
                                       stringByReplacingOccurrencesOfString:@"\"" withString:@"\\\""]
                                       stringByReplacingOccurrencesOfString:@"\n" withString:@" "];

        NSMutableString *scriptStr = [NSMutableString stringWithFormat:@"display notification \"%@\" with title \"%@\"", escBody, escTitle];
        if (nsSub.length > 0) {
            NSString *escSub = [[[nsSub stringByReplacingOccurrencesOfString:@"\\" withString:@"\\\\"]
                                       stringByReplacingOccurrencesOfString:@"\"" withString:@"\\\""]
                                       stringByReplacingOccurrencesOfString:@"\n" withString:@" "];
            [scriptStr appendFormat:@" subtitle \"%@\"", escSub];
        }

        NSAppleScript *appleScript = [[NSAppleScript alloc] initWithSource:scriptStr];
        NSDictionary *errorInfo = nil;
        [appleScript executeAndReturnError:&errorInfo];
        return 0;
    }
}

// Coordinate space of the primary display selected by the capture stream.
void gm_mac_primary_display_bounds(double *x, double *y, double *w, double *h) {
    CGRect r = CGDisplayBounds(CGMainDisplayID());
    *x = r.origin.x; *y = r.origin.y; *w = r.size.width; *h = r.size.height;
}
