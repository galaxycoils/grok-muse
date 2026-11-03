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
static os_unfair_lock g_frame_lock = OS_UNFAIR_LOCK_INIT;
static SCStream *g_stream = nil;

@interface GmStreamOutputDelegate : NSObject <SCStreamOutput, SCStreamDelegate>
@end

@implementation GmStreamOutputDelegate
- (void)stream:(SCStream *)stream didOutputSampleBuffer:(CMSampleBufferRef)sampleBuffer ofType:(SCStreamOutputType)type {
    if (type != SCStreamOutputTypeScreen) return;
    
    CVPixelBufferRef pixelBuffer = CMSampleBufferGetImageBuffer(sampleBuffer);
    if (!pixelBuffer) return;
    
    CVPixelBufferLockBaseAddress(pixelBuffer, kCVPixelBufferLock_ReadOnly);
    size_t width = CVPixelBufferGetWidth(pixelBuffer);
    size_t height = CVPixelBufferGetHeight(pixelBuffer);
    size_t bytesPerRow = CVPixelBufferGetBytesPerRow(pixelBuffer);
    const uint8_t *baseAddress = (const uint8_t *)CVPixelBufferGetBaseAddress(pixelBuffer);
    size_t totalBytes = bytesPerRow * height;
    
    if (baseAddress && totalBytes > 0) {
        uint8_t *new_buf = (uint8_t *)malloc(totalBytes);
        if (new_buf) {
            memcpy(new_buf, baseAddress, totalBytes);
            
            os_unfair_lock_lock(&g_frame_lock);
            if (g_latest_frame.data) {
                free(g_latest_frame.data);
            }
            g_latest_frame.data = new_buf;
            g_latest_frame.len = totalBytes;
            g_latest_frame.width = (uint32_t)width;
            g_latest_frame.height = (uint32_t)height;
            g_latest_frame.scale = 2.0;
            g_latest_frame.timestamp_ms = (uint64_t)([[NSDate date] timeIntervalSince1970] * 1000.0);
            os_unfair_lock_unlock(&g_frame_lock);
        }
    }
    
    CVPixelBufferUnlockBaseAddress(pixelBuffer, kCVPixelBufferLock_ReadOnly);
}
@end

static GmStreamOutputDelegate *g_stream_delegate = nil;

int gm_mac_check_screen_recording_permission(void) {
    return CGPreflightScreenCaptureAccess() ? 1 : 0;
}

int gm_mac_check_ax_permission(void) {
    return AXIsProcessTrusted() ? 1 : 0;
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
            
            SCDisplay *display = content.displays.firstObject;
            SCContentFilter *filter = [[SCContentFilter alloc] initWithDisplay:display excludingWindows:@[]];
            SCStreamConfiguration *config = [[SCStreamConfiguration alloc] init];
            config.width = display.width > 0 ? display.width : 1680;
            config.height = display.height > 0 ? display.height : 1050;
            config.minimumFrameInterval = CMTimeMake(1, 30);
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

int gm_mac_get_latest_frame(uint8_t **out_data, size_t *out_len, uint32_t *out_w, uint32_t *out_h, double *out_scale, uint64_t *out_ts) {
    os_unfair_lock_lock(&g_frame_lock);
    if (g_latest_frame.data && g_latest_frame.len > 0) {
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
            return 0; // Success from persistent stream (< 2ms)
        }
    }
    os_unfair_lock_unlock(&g_frame_lock);
    
    // Fallback: capture on-demand via ScreenCaptureKit
    @autoreleasepool {
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block CGImageRef image = NULL;
        
        [SCShareableContent getShareableContentWithCompletionHandler:^(SCShareableContent *content, NSError *error) {
            if (error || !content || content.displays.count == 0) {
                dispatch_semaphore_signal(sem);
                return;
            }
            SCDisplay *display = content.displays.firstObject;
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
    if (g_stream) {
        [g_stream stopCaptureWithCompletionHandler:nil];
        g_stream = nil;
        g_stream_delegate = nil;
    }
    os_unfair_lock_lock(&g_frame_lock);
    if (g_latest_frame.data) {
        free(g_latest_frame.data);
        g_latest_frame.data = NULL;
        g_latest_frame.len = 0;
    }
    os_unfair_lock_unlock(&g_frame_lock);
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
