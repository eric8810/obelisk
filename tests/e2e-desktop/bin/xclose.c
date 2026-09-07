// Close a window via the ICCCM WM_DELETE_WINDOW protocol (the same path a
// titlebar X button takes). Usage: xclose <window-id-hex-or-decimal>
// Built on demand by the desktop E2E driver (tests/e2e-desktop).
#include <X11/Xlib.h>
#include <X11/Xatom.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char **argv) {
    if (argc < 2) { fprintf(stderr, "usage: %s <window>\n", argv[0]); return 2; }
    Display *d = XOpenDisplay(NULL);
    if (!d) { fprintf(stderr, "cannot open display\n"); return 1; }
    Window w = strtoul(argv[1], NULL, 0);
    Atom protocols = XInternAtom(d, "WM_PROTOCOLS", False);
    Atom delete_window = XInternAtom(d, "WM_DELETE_WINDOW", False);
    XEvent event = {0};
    event.type = ClientMessage;
    event.xclient.window = w;
    event.xclient.message_type = protocols;
    event.xclient.format = 32;
    event.xclient.data.l[0] = delete_window;
    event.xclient.data.l[1] = CurrentTime;
    XSendEvent(d, w, False, NoEventMask, &event);
    XFlush(d);
    XCloseDisplay(d);
    return 0;
}
