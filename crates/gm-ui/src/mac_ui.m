#import <AppKit/AppKit.h>

static NSStatusItem *g_status_item = nil;
static NSWindow *g_chat_window = nil;

int gm_mac_ui_init_tray(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        
        g_status_item = [NSStatusBar.systemStatusBar statusItemWithLength:NSVariableStatusItemLength];
        if (g_status_item) {
            g_status_item.button.title = @"🤖 Grok-Muse";
            NSMenu *menu = [[NSMenu alloc] initWithTitle:@"Grok-Muse"];
            [menu addItemWithTitle:@"Open Chat" action:nil keyEquivalent:@""];
            [menu addItem:[NSMenuItem separatorItem]];
            [menu addItemWithTitle:@"Emergency Kill Switch" action:nil keyEquivalent:@""];
            [menu addItemWithTitle:@"Quit" action:@selector(terminate:) keyEquivalent:@"q"];
            g_status_item.menu = menu;
            return 0;
        }
        return -1;
    }
}

int gm_mac_ui_init_chat_window(void) {
    @autoreleasepool {
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
        
        NSRect frame = NSMakeRect(200, 200, 480, 600);
        NSUInteger styleMask = NSWindowStyleMaskTitled | NSWindowStyleMaskClosable | NSWindowStyleMaskResizable;
        g_chat_window = [[NSWindow alloc] initWithContentRect:frame
                                                    styleMask:styleMask
                                                      backing:NSBackingStoreBuffered
                                                        defer:NO];
        if (g_chat_window) {
            [g_chat_window setTitle:@"Grok-Muse Assistant"];
            NSTextView *textView = [[NSTextView alloc] initWithFrame:NSMakeRect(10, 50, 460, 500)];
            [textView setString:@"How can I assist you on macOS today?"];
            [textView setEditable:NO];
            [[g_chat_window contentView] addSubview:textView];
            
            NSTextField *inputField = [[NSTextField alloc] initWithFrame:NSMakeRect(10, 10, 460, 30)];
            [inputField setPlaceholderString:@"Enter instruction..."];
            [[g_chat_window contentView] addSubview:inputField];
            
            [g_chat_window makeKeyAndOrderFront:nil];
            return 0;
        }
        return -1;
    }
}
