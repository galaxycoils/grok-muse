#import <AppKit/AppKit.h>
#import <Carbon/Carbon.h>
#import <EventKit/EventKit.h>
#import <ApplicationServices/ApplicationServices.h>

// Forward declarations of C callbacks
typedef void (*GmHotkeyCallback)(int hotkey_id);
typedef void (*GmTaskSubmittedCallback)(const char *instruction);
typedef void (*GmPauseToggledCallback)(int is_paused);

static GmHotkeyCallback g_hotkey_callback = NULL;
static GmTaskSubmittedCallback g_task_callback = NULL;
static GmPauseToggledCallback g_pause_callback = NULL;

static NSStatusItem *g_status_item = nil;
static NSWindow *g_chat_window = nil;
static NSTextView *g_chat_transcript = nil;
static NSTextField *g_chat_input = nil;

static NSPanel *g_overlay_panel = nil;
static NSTextField *g_overlay_title = nil;
static NSTextField *g_overlay_detail = nil;
static NSView *g_overlay_indicator = nil;

static NSWindow *g_onboarding_window = nil;
static NSTextField *g_lbl_screen = nil;
static NSTextField *g_lbl_ax = nil;
static NSTextField *g_lbl_cal = nil;
static NSTextField *g_lbl_rem = nil;

static BOOL g_is_paused = NO;
static int g_current_overlay_state = 0; // 0=Idle, 1=Acting, 2=Paused, 3=Stopped

// Carbon Hotkey Refs
static EventHandlerRef g_carbon_ui_handler = NULL;
static EventHotKeyRef g_ui_summon_hotkey = NULL;
static EventHotKeyRef g_ui_kill_hotkey = NULL;

// Function forward declarations
int gm_mac_ui_init_chat_window(void);
int gm_mac_ui_show_chat_window(void);
int gm_mac_ui_show_onboarding(void);
void gm_mac_ui_refresh_onboarding_permissions(void);
void gm_mac_ui_set_overlay_state(int state, const char *status_text, const char *detail_text);

@interface GmUiController : NSObject
- (void)onSummon:(id)sender;
- (void)onTogglePause:(id)sender;
- (void)onRunTask:(id)sender;
- (void)onOpenPermissions:(id)sender;
- (void)onEmergencyKill:(id)sender;
- (void)onOpenScreenSettings:(id)sender;
- (void)onOpenAxSettings:(id)sender;
- (void)onOpenCalSettings:(id)sender;
- (void)onOpenRemSettings:(id)sender;
- (void)onRefreshPermissions:(id)sender;
- (void)onCloseOnboarding:(id)sender;
- (void)onSendChat:(id)sender;
- (void)openSettingsPane:(NSString *)pane;
@end

static GmUiController *g_ui_controller = nil;

@implementation GmUiController

- (void)openSettingsPane:(NSString *)pane {
    NSString *urlStr = [NSString stringWithFormat:@"x-apple.systempreferences:com.apple.preference.security?%@", pane];
    NSURL *url = [NSURL URLWithString:urlStr];
    if (url) {
        [[NSWorkspace sharedWorkspace] openURL:url];
    }
}

- (void)onSummon:(id)sender {
    (void)sender;
    gm_mac_ui_show_chat_window();
    if (g_hotkey_callback) {
        g_hotkey_callback(1); // 1 = Summon
    }
}

- (void)onTogglePause:(id)sender {
    (void)sender;
    g_is_paused = !g_is_paused;
    if (g_is_paused) {
        gm_mac_ui_set_overlay_state(2, "⏸ Grok-Muse: Paused", "Agent paused by user coexistence toggle");
    } else {
        gm_mac_ui_set_overlay_state(0, "⚪️ Grok-Muse: Idle", "Ready · Cmd+Shift+Space to summon");
    }
    
    // Update tray menu item
    if (g_status_item && g_status_item.menu) {
        NSMenuItem *pauseItem = [g_status_item.menu itemWithTag:202];
        if (pauseItem) {
            [pauseItem setTitle:g_is_paused ? @"Resume Agent" : @"Pause Agent"];
        }
        NSMenuItem *statusItem = [g_status_item.menu itemWithTag:201];
        if (statusItem) {
            [statusItem setTitle:g_is_paused ? @"Status: Paused" : @"Status: Idle"];
        }
    }
    
    if (g_pause_callback) {
        g_pause_callback(g_is_paused ? 1 : 0);
    }
}

- (void)onRunTask:(id)sender {
    (void)sender;
    gm_mac_ui_show_chat_window();
    if (g_chat_input) {
        [g_chat_window makeFirstResponder:g_chat_input];
    }
}

- (void)onOpenPermissions:(id)sender {
    (void)sender;
    gm_mac_ui_show_onboarding();
}

- (void)onEmergencyKill:(id)sender {
    (void)sender;
    gm_mac_ui_set_overlay_state(3, "🛑 Grok-Muse: Stopped", "Emergency kill switch activated (Cmd+Alt+Escape)");
    if (g_status_item && g_status_item.menu) {
        NSMenuItem *statusItem = [g_status_item.menu itemWithTag:201];
        if (statusItem) {
            [statusItem setTitle:@"Status: Stopped (Kill Switch)"];
        }
    }
    if (g_hotkey_callback) {
        g_hotkey_callback(2); // 2 = Kill Switch
    }
}

- (void)onOpenScreenSettings:(id)sender {
    (void)sender;
    [self openSettingsPane:@"Privacy_ScreenCapture"];
}

- (void)onOpenAxSettings:(id)sender {
    (void)sender;
    [self openSettingsPane:@"Privacy_Accessibility"];
}

- (void)onOpenCalSettings:(id)sender {
    (void)sender;
    [self openSettingsPane:@"Privacy_Calendars"];
}

- (void)onOpenRemSettings:(id)sender {
    (void)sender;
    [self openSettingsPane:@"Privacy_Reminders"];
}

- (void)onRefreshPermissions:(id)sender {
    (void)sender;
    gm_mac_ui_refresh_onboarding_permissions();
}

- (void)onCloseOnboarding:(id)sender {
    (void)sender;
    if (g_onboarding_window) {
        [g_onboarding_window orderOut:nil];
    }
}

- (void)onSendChat:(id)sender {
    (void)sender;
    if (!g_chat_input) return;
    NSString *input = [g_chat_input stringValue];
    if ([input length] == 0) return;
    
    if (g_chat_transcript) {
        NSString *prev = [g_chat_transcript string];
        NSString *updated = [NSString stringWithFormat:@"%@\n\nUser: %@", prev, input];
        [g_chat_transcript setString:updated];
        [g_chat_transcript scrollRangeToVisible:NSMakeRange([updated length], 0)];
    }
    [g_chat_input setStringValue:@""];
    
    // Change overlay to acting
    gm_mac_ui_set_overlay_state(1, "⚡️ Grok-Muse: Acting", [input UTF8String]);
    
    if (g_task_callback) {
        g_task_callback([input UTF8String]);
    }
}

@end

// Carbon Hotkey Handler
static OSStatus GmCarbonUiHotkeyHandler(EventHandlerCallRef nextHandler, EventRef theEvent, void *userData) {
    (void)nextHandler;
    (void)userData;
    EventHotKeyID hotKeyID;
    OSStatus err = GetEventParameter(theEvent, kEventParamDirectObject, typeEventHotKeyID, NULL, sizeof(hotKeyID), NULL, &hotKeyID);
    if (err == noErr && g_ui_controller) {
        if (hotKeyID.id == 1) {
            dispatch_async(dispatch_get_main_queue(), ^{
                [g_ui_controller onSummon:nil];
            });
        } else if (hotKeyID.id == 2) {
            dispatch_async(dispatch_get_main_queue(), ^{
                [g_ui_controller onEmergencyKill:nil];
            });
        }
    }
    return noErr;
}

int gm_mac_ui_init_global_hotkeys(void) {
    if (g_carbon_ui_handler) return 0;
    
    EventTypeSpec eventType;
    eventType.eventClass = kEventClassKeyboard;
    eventType.eventKind = kEventHotKeyPressed;
    OSStatus status = InstallApplicationEventHandler(&GmCarbonUiHotkeyHandler, 1, &eventType, NULL, &g_carbon_ui_handler);
    if (status != noErr) {
        return (int)status;
    }
    
    // Summon: Cmd + Shift + Space (Space keycode 49)
    EventHotKeyID summonID = { 'GMUI', 1 };
    RegisterEventHotKey(49, cmdKey | shiftKey, summonID, GetApplicationEventTarget(), 0, &g_ui_summon_hotkey);
    
    // Kill switch: Cmd + Option + Escape (Escape keycode 53)
    EventHotKeyID killID = { 'GMUI', 2 };
    RegisterEventHotKey(53, cmdKey | optionKey, killID, GetApplicationEventTarget(), 0, &g_ui_kill_hotkey);
    
    // Also install Cocoa global monitor for redundancy
    [NSEvent addGlobalMonitorForEventsMatchingMask:NSEventMaskKeyDown handler:^(NSEvent *event) {
        NSUInteger flags = [event modifierFlags] & NSEventModifierFlagDeviceIndependentFlagsMask;
        if (event.keyCode == 49 && (flags == (NSEventModifierFlagCommand | NSEventModifierFlagShift))) {
            dispatch_async(dispatch_get_main_queue(), ^{
                [g_ui_controller onSummon:nil];
            });
        } else if (event.keyCode == 53 && (flags == (NSEventModifierFlagCommand | NSEventModifierFlagOption))) {
            dispatch_async(dispatch_get_main_queue(), ^{
                [g_ui_controller onEmergencyKill:nil];
            });
        }
    }];
    
    return 0;
}

int gm_mac_ui_init_action_overlay(void) {
    @autoreleasepool {
        if (g_overlay_panel) return 0;
        
        NSRect screenRect = [[NSScreen mainScreen] frame];
        CGFloat width = 290.0;
        CGFloat height = 56.0;
        CGFloat x = screenRect.origin.x + screenRect.size.width - width - 20.0;
        CGFloat y = screenRect.origin.y + screenRect.size.height - height - 35.0;
        
        NSRect frame = NSMakeRect(x, y, width, height);
        NSPanel *panel = [[NSPanel alloc] initWithContentRect:frame
                                                    styleMask:NSWindowStyleMaskNonactivatingPanel | NSWindowStyleMaskHUDWindow | NSWindowStyleMaskBorderless
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        [panel setLevel:NSFloatingWindowLevel];
        [panel setOpaque:NO];
        [panel setBackgroundColor:[NSColor colorWithCalibratedRed:0.12 green:0.12 blue:0.14 alpha:0.92]];
        [panel setHasShadow:YES];
        [panel setMovableByWindowBackground:YES];
        
        NSView *cv = [panel contentView];
        cv.wantsLayer = YES;
        cv.layer.cornerRadius = 14.0;
        cv.layer.masksToBounds = YES;
        cv.layer.borderColor = [[NSColor colorWithCalibratedWhite:0.3 alpha:0.6] CGColor];
        cv.layer.borderWidth = 1.0;
        
        // Indicator dot
        g_overlay_indicator = [[NSView alloc] initWithFrame:NSMakeRect(14, 21, 14, 14)];
        g_overlay_indicator.wantsLayer = YES;
        g_overlay_indicator.layer.cornerRadius = 7.0;
        g_overlay_indicator.layer.backgroundColor = [[NSColor systemGrayColor] CGColor];
        [cv addSubview:g_overlay_indicator];
        
        // Title label
        g_overlay_title = [[NSTextField alloc] initWithFrame:NSMakeRect(36, 28, 240, 20)];
        [g_overlay_title setEditable:NO];
        [g_overlay_title setSelectable:NO];
        [g_overlay_title setBordered:NO];
        [g_overlay_title setDrawsBackground:NO];
        [g_overlay_title setFont:[NSFont boldSystemFontOfSize:13.0]];
        [g_overlay_title setTextColor:[NSColor whiteColor]];
        [g_overlay_title setStringValue:@"Grok-Muse: Idle"];
        [cv addSubview:g_overlay_title];
        
        // Detail label
        g_overlay_detail = [[NSTextField alloc] initWithFrame:NSMakeRect(36, 10, 240, 16)];
        [g_overlay_detail setEditable:NO];
        [g_overlay_detail setSelectable:NO];
        [g_overlay_detail setBordered:NO];
        [g_overlay_detail setDrawsBackground:NO];
        [g_overlay_detail setFont:[NSFont systemFontOfSize:11.0]];
        [g_overlay_detail setTextColor:[NSColor lightGrayColor]];
        [g_overlay_detail setStringValue:@"Ready · Cmd+Shift+Space to summon"];
        [cv addSubview:g_overlay_detail];
        
        [panel orderFrontRegardless];
        g_overlay_panel = panel;
        return 0;
    }
}

void gm_mac_ui_set_overlay_state(int state, const char *status_text, const char *detail_text) {
    @autoreleasepool {
        if (!g_overlay_panel) {
            gm_mac_ui_init_action_overlay();
        }
        g_current_overlay_state = state;
        NSString *statusStr = status_text ? [NSString stringWithUTF8String:status_text] : nil;
        NSString *detailStr = detail_text ? [NSString stringWithUTF8String:detail_text] : nil;
        
        dispatch_async(dispatch_get_main_queue(), ^{
            switch (state) {
                case 0: // Idle
                    g_overlay_indicator.layer.backgroundColor = [[NSColor systemGrayColor] CGColor];
                    [g_overlay_title setStringValue:statusStr ?: @"Grok-Muse: Idle"];
                    [g_overlay_detail setStringValue:detailStr ?: @"Ready · Cmd+Shift+Space to summon"];
                    break;
                case 1: // Acting
                    g_overlay_indicator.layer.backgroundColor = [[NSColor colorWithCalibratedRed:0.2 green:0.6 blue:1.0 alpha:1.0] CGColor];
                    [g_overlay_title setStringValue:statusStr ?: @"⚡️ Grok-Muse: Acting"];
                    [g_overlay_detail setStringValue:detailStr ?: @"Executing computer-use action..."];
                    break;
                case 2: // Paused
                    g_overlay_indicator.layer.backgroundColor = [[NSColor colorWithCalibratedRed:1.0 green:0.75 blue:0.1 alpha:1.0] CGColor];
                    [g_overlay_title setStringValue:statusStr ?: @"⏸ Grok-Muse: Paused"];
                    [g_overlay_detail setStringValue:detailStr ?: @"User coexistence active · waiting for idle"];
                    break;
                case 3: // Stopped
                    g_overlay_indicator.layer.backgroundColor = [[NSColor systemRedColor] CGColor];
                    [g_overlay_title setStringValue:statusStr ?: @"🛑 Grok-Muse: Stopped"];
                    [g_overlay_detail setStringValue:detailStr ?: @"Emergency kill switch triggered"];
                    break;
            }
            [g_overlay_panel orderFrontRegardless];
        });
    }
}

void gm_mac_ui_show_overlay(int show) {
    if (!g_overlay_panel) {
        gm_mac_ui_init_action_overlay();
    }
    dispatch_async(dispatch_get_main_queue(), ^{
        if (show) {
            [g_overlay_panel orderFrontRegardless];
        } else {
            [g_overlay_panel orderOut:nil];
        }
    });
}

void gm_mac_ui_refresh_onboarding_permissions(void) {
    if (!g_lbl_screen) return;
    
    BOOL screenOk = CGPreflightScreenCaptureAccess();
    BOOL axOk = AXIsProcessTrusted();
    
    #pragma clang diagnostic push
    #pragma clang diagnostic ignored "-Wdeprecated-declarations"
    EKAuthorizationStatus calStatus = [EKEventStore authorizationStatusForEntityType:EKEntityTypeEvent];
    BOOL calOk = (calStatus >= 3);
    EKAuthorizationStatus remStatus = [EKEventStore authorizationStatusForEntityType:EKEntityTypeReminder];
    BOOL remOk = (remStatus >= 3);
    #pragma clang diagnostic pop
    
    dispatch_async(dispatch_get_main_queue(), ^{
        [g_lbl_screen setStringValue:screenOk ? @"Screen Recording:   🟢 Granted" : @"Screen Recording:   🔴 Not Granted"];
        [g_lbl_screen setTextColor:screenOk ? [NSColor systemGreenColor] : [NSColor systemRedColor]];
        
        [g_lbl_ax setStringValue:axOk ? @"Accessibility:        🟢 Granted" : @"Accessibility:        🔴 Not Granted"];
        [g_lbl_ax setTextColor:axOk ? [NSColor systemGreenColor] : [NSColor systemRedColor]];
        
        [g_lbl_cal setStringValue:calOk ? @"Calendars:           🟢 Granted" : @"Calendars:           🔴 Not Granted"];
        [g_lbl_cal setTextColor:calOk ? [NSColor systemGreenColor] : [NSColor systemRedColor]];
        
        [g_lbl_rem setStringValue:remOk ? @"Reminders:           🟢 Granted" : @"Reminders:           🔴 Not Granted"];
        [g_lbl_rem setTextColor:remOk ? [NSColor systemGreenColor] : [NSColor systemRedColor]];
    });
}

int gm_mac_ui_show_onboarding(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        if (!g_ui_controller) {
            g_ui_controller = [[GmUiController alloc] init];
        }
        
        if (g_onboarding_window) {
            [g_onboarding_window makeKeyAndOrderFront:nil];
            gm_mac_ui_refresh_onboarding_permissions();
            return 0;
        }
        
        NSRect frame = NSMakeRect(250, 200, 540, 440);
        NSUInteger styleMask = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable;
        g_onboarding_window = [[NSWindow alloc] initWithContentRect:frame
                                                          styleMask:styleMask
                                                            backing:NSBackingStoreBuffered
                                                              defer:NO];
        if (!g_onboarding_window) return -1;
        
        [g_onboarding_window setTitle:@"Grok-Muse Permissions & Setup"];
        [g_onboarding_window center];
        
        NSView *cv = [g_onboarding_window contentView];
        
        // Header
        NSTextField *title = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 390, 500, 30)];
        [title setEditable:NO];
        [title setBordered:NO];
        [title setDrawsBackground:NO];
        [title setFont:[NSFont boldSystemFontOfSize:18.0]];
        [title setStringValue:@"Grok-Muse System Permissions"];
        [cv addSubview:title];
        
        NSTextField *desc = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 335, 500, 45)];
        [desc setEditable:NO];
        [desc setBordered:NO];
        [desc setDrawsBackground:NO];
        [desc setFont:[NSFont systemFontOfSize:12.0]];
        [desc setTextColor:[NSColor secondaryLabelColor]];
        [desc setStringValue:@"To enable fast computer-use and proactive assistance, Grok-Muse requires permissions in macOS System Settings. Grant each permission below, then click Refresh Status."];
        [cv addSubview:desc];
        
        // Row 1: Screen Recording
        g_lbl_screen = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 280, 330, 24)];
        [g_lbl_screen setEditable:NO];
        [g_lbl_screen setBordered:NO];
        [g_lbl_screen setDrawsBackground:NO];
        [g_lbl_screen setFont:[NSFont boldSystemFontOfSize:13.0]];
        [cv addSubview:g_lbl_screen];
        
        NSButton *btnScreen = [[NSButton alloc] initWithFrame:NSMakeRect(360, 276, 150, 30)];
        [btnScreen setTitle:@"Open Settings..."];
        [btnScreen setBezelStyle:NSBezelStyleRounded];
        [btnScreen setTarget:g_ui_controller];
        [btnScreen setAction:@selector(onOpenScreenSettings:)];
        [cv addSubview:btnScreen];
        
        // Row 2: Accessibility
        g_lbl_ax = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 230, 330, 24)];
        [g_lbl_ax setEditable:NO];
        [g_lbl_ax setBordered:NO];
        [g_lbl_ax setDrawsBackground:NO];
        [g_lbl_ax setFont:[NSFont boldSystemFontOfSize:13.0]];
        [cv addSubview:g_lbl_ax];
        
        NSButton *btnAx = [[NSButton alloc] initWithFrame:NSMakeRect(360, 226, 150, 30)];
        [btnAx setTitle:@"Open Settings..."];
        [btnAx setBezelStyle:NSBezelStyleRounded];
        [btnAx setTarget:g_ui_controller];
        [btnAx setAction:@selector(onOpenAxSettings:)];
        [cv addSubview:btnAx];
        
        // Row 3: Calendars
        g_lbl_cal = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 180, 330, 24)];
        [g_lbl_cal setEditable:NO];
        [g_lbl_cal setBordered:NO];
        [g_lbl_cal setDrawsBackground:NO];
        [g_lbl_cal setFont:[NSFont boldSystemFontOfSize:13.0]];
        [cv addSubview:g_lbl_cal];
        
        NSButton *btnCal = [[NSButton alloc] initWithFrame:NSMakeRect(360, 176, 150, 30)];
        [btnCal setTitle:@"Open Settings..."];
        [btnCal setBezelStyle:NSBezelStyleRounded];
        [btnCal setTarget:g_ui_controller];
        [btnCal setAction:@selector(onOpenCalSettings:)];
        [cv addSubview:btnCal];
        
        // Row 4: Reminders
        g_lbl_rem = [[NSTextField alloc] initWithFrame:NSMakeRect(20, 130, 330, 24)];
        [g_lbl_rem setEditable:NO];
        [g_lbl_rem setBordered:NO];
        [g_lbl_rem setDrawsBackground:NO];
        [g_lbl_rem setFont:[NSFont boldSystemFontOfSize:13.0]];
        [cv addSubview:g_lbl_rem];
        
        NSButton *btnRem = [[NSButton alloc] initWithFrame:NSMakeRect(360, 126, 150, 30)];
        [btnRem setTitle:@"Open Settings..."];
        [btnRem setBezelStyle:NSBezelStyleRounded];
        [btnRem setTarget:g_ui_controller];
        [btnRem setAction:@selector(onOpenRemSettings:)];
        [cv addSubview:btnRem];
        
        // Bottom buttons
        NSButton *btnRefresh = [[NSButton alloc] initWithFrame:NSMakeRect(20, 30, 160, 36)];
        [btnRefresh setTitle:@"Refresh Status"];
        [btnRefresh setBezelStyle:NSBezelStyleRounded];
        [btnRefresh setTarget:g_ui_controller];
        [btnRefresh setAction:@selector(onRefreshPermissions:)];
        [cv addSubview:btnRefresh];
        
        NSButton *btnClose = [[NSButton alloc] initWithFrame:NSMakeRect(380, 30, 130, 36)];
        [btnClose setTitle:@"Done"];
        [btnClose setBezelStyle:NSBezelStyleRounded];
        [btnClose setTarget:g_ui_controller];
        [btnClose setAction:@selector(onCloseOnboarding:)];
        [cv addSubview:btnClose];
        
        gm_mac_ui_refresh_onboarding_permissions();
        [g_onboarding_window makeKeyAndOrderFront:nil];
        return 0;
    }
}

int gm_mac_ui_init_chat_window(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        if (!g_ui_controller) {
            g_ui_controller = [[GmUiController alloc] init];
        }
        
        if (g_chat_window) {
            return 0;
        }
        
        NSRect frame = NSMakeRect(200, 200, 500, 600);
        NSUInteger styleMask = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable;
        g_chat_window = [[NSWindow alloc] initWithContentRect:frame
                                                    styleMask:styleMask
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        if (!g_chat_window) return -1;
        
        [g_chat_window setTitle:@"Grok-Muse Assistant"];
        [g_chat_window center];
        
        NSView *cv = [g_chat_window contentView];
        
        // Transcript view in scrollview
        NSScrollView *scrollView = [[NSScrollView alloc] initWithFrame:NSMakeRect(10, 60, 480, 530)];
        [scrollView setHasVerticalScroller:YES];
        [scrollView setAutoresizingMask:NSViewWidthSizable | NSViewHeightSizable];
        
        g_chat_transcript = [[NSTextView alloc] initWithFrame:scrollView.bounds];
        [g_chat_transcript setEditable:NO];
        [g_chat_transcript setFont:[NSFont systemFontOfSize:13.0]];
        [g_chat_transcript setString:@"🤖 Grok-Muse Assistant Ready.\n\nPress Cmd+Shift+Space to summon at any time, or Cmd+Alt+Escape for the emergency kill switch.\n\nType a computer-use task below (e.g. 'Open Safari and search for Rust 2024'):"];
        [scrollView setDocumentView:g_chat_transcript];
        [cv addSubview:scrollView];
        
        // Input text field
        g_chat_input = [[NSTextField alloc] initWithFrame:NSMakeRect(10, 15, 380, 32)];
        [g_chat_input setPlaceholderString:@"Enter task instruction..."];
        [g_chat_input setTarget:g_ui_controller];
        [g_chat_input setAction:@selector(onSendChat:)];
        [g_chat_input setAutoresizingMask:NSViewWidthSizable | NSViewMaxYMargin];
        [cv addSubview:g_chat_input];
        
        // Send button
        NSButton *sendBtn = [[NSButton alloc] initWithFrame:NSMakeRect(400, 14, 90, 34)];
        [sendBtn setTitle:@"Run Task"];
        [sendBtn setBezelStyle:NSBezelStyleRounded];
        [sendBtn setTarget:g_ui_controller];
        [sendBtn setAction:@selector(onSendChat:)];
        [sendBtn setAutoresizingMask:NSViewMinXMargin | NSViewMaxYMargin];
        [cv addSubview:sendBtn];
        
        return 0;
    }
}

int gm_mac_ui_show_chat_window(void) {
    if (!g_chat_window) {
        gm_mac_ui_init_chat_window();
    }
    dispatch_async(dispatch_get_main_queue(), ^{
        [NSApp activateIgnoringOtherApps:YES];
        [g_chat_window makeKeyAndOrderFront:nil];
        if (g_chat_input) {
            [g_chat_window makeFirstResponder:g_chat_input];
        }
    });
    return 0;
}

int gm_mac_ui_init_tray(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        
        if (!g_ui_controller) {
            g_ui_controller = [[GmUiController alloc] init];
        }
        
        g_status_item = [NSStatusBar.systemStatusBar statusItemWithLength:NSVariableStatusItemLength];
        if (!g_status_item) return -1;
        
        g_status_item.button.title = @"🤖 Grok-Muse";
        
        NSMenu *menu = [[NSMenu alloc] initWithTitle:@"Grok-Muse"];
        
        // 1. Header / title
        NSMenuItem *header = [[NSMenuItem alloc] initWithTitle:@"Grok-Muse Hybrid Agent" action:nil keyEquivalent:@""];
        [header setEnabled:NO];
        [menu addItem:header];
        
        [menu addItem:[NSMenuItem separatorItem]];
        
        // 2. Summon option
        NSMenuItem *summonItem = [[NSMenuItem alloc] initWithTitle:@"Summon Assistant (Cmd+Shift+Space)"
                                                           action:@selector(onSummon:)
                                                    keyEquivalent:@""];
        [summonItem setTarget:g_ui_controller];
        [menu addItem:summonItem];
        
        // 3. Status indicator
        NSMenuItem *statusItem = [[NSMenuItem alloc] initWithTitle:@"Status: Idle" action:nil keyEquivalent:@""];
        [statusItem setTag:201];
        [statusItem setEnabled:NO];
        [menu addItem:statusItem];
        
        // 4. Pause / Resume coexistence
        NSMenuItem *pauseItem = [[NSMenuItem alloc] initWithTitle:@"Pause Agent"
                                                          action:@selector(onTogglePause:)
                                                   keyEquivalent:@""];
        [pauseItem setTag:202];
        [pauseItem setTarget:g_ui_controller];
        [menu addItem:pauseItem];
        
        // 5. Run Task
        NSMenuItem *runTaskItem = [[NSMenuItem alloc] initWithTitle:@"Run Task..."
                                                            action:@selector(onRunTask:)
                                                     keyEquivalent:@"t"];
        [runTaskItem setTarget:g_ui_controller];
        [menu addItem:runTaskItem];
        
        // 6. Permissions & Onboarding
        NSMenuItem *permItem = [[NSMenuItem alloc] initWithTitle:@"Permissions & Onboarding..."
                                                         action:@selector(onOpenPermissions:)
                                                  keyEquivalent:@"p"];
        [permItem setTarget:g_ui_controller];
        [menu addItem:permItem];
        
        [menu addItem:[NSMenuItem separatorItem]];
        
        // 7. Emergency Kill Switch
        NSMenuItem *killItem = [[NSMenuItem alloc] initWithTitle:@"Emergency Kill Switch (Cmd+Alt+Escape)"
                                                         action:@selector(onEmergencyKill:)
                                                  keyEquivalent:@""];
        [killItem setTarget:g_ui_controller];
        [menu addItem:killItem];
        
        [menu addItem:[NSMenuItem separatorItem]];
        
        // 8. Quit
        NSMenuItem *quitItem = [[NSMenuItem alloc] initWithTitle:@"Quit Grok-Muse"
                                                         action:@selector(terminate:)
                                                  keyEquivalent:@"q"];
        [menu addItem:quitItem];
        
        g_status_item.menu = menu;
        
        // Initialize floating action overlay and hotkeys
        gm_mac_ui_init_action_overlay();
        gm_mac_ui_init_global_hotkeys();
        
        return 0;
    }
}

void gm_mac_ui_set_callbacks(GmHotkeyCallback hotkey_cb, GmTaskSubmittedCallback task_cb, GmPauseToggledCallback pause_cb) {
    g_hotkey_callback = hotkey_cb;
    g_task_callback = task_cb;
    g_pause_callback = pause_cb;
}
