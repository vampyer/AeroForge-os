/* fakepad: a simulated USB gamepad for testing AeroForge in QEMU, which has
 * no gamepad device of its own.
 *
 * Like tools/fakebt it speaks the usbredir protocol (the "USB host" side) on
 * a Unix socket, and QEMU attaches it to the guest's xHCI controller with
 *   -chardev socket,id=pad,path=build/pad.sock -device usb-redir,chardev=pad,bus=xhci.0,port=4
 *
 * --xinput: an Xbox 360 style wired pad (045e:028e, interface class
 * FF/5D/01, as most 2.4 GHz dongles present themselves). It first sends an
 * LED status message, which the host must skip, then the same input report
 * every second: A, Y, LB and Start held, d-pad down-right, left stick right
 * and up, right stick left, left trigger fully pressed. It logs the LED
 * command the host sends ("LED: player 1").
 *
 * --hid: a generic HID gamepad (0079:0006) whose first interface is a
 * consumer control (volume keys) that is not a gamepad, so the host has to
 * read the report descriptors to find the right one. Its gamepad reports use
 * report ID 1: X at minimum, Y centred, Z at maximum, Rz centred, hat left,
 * buttons 2 and 10 held.
 *
 * Build: cc -O2 -o build/fakepad tools/fakepad/fakepad.c -lusbredirparser
 * Run:   build/fakepad <socket> --xinput|--hid */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include <usbredirparser.h>

static struct usbredirparser *parser;
static int sock = -1;
static int xinput;
static int reports_started;
static double next_report;
static int reports_sent;

static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "fakepad(%s): ", xinput ? "xinput" : "hid");
    vfprintf(stderr, fmt, ap);
    fprintf(stderr, "\n");
    va_end(ap);
}

static double now(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC, &t);
    return t.tv_sec + t.tv_nsec / 1e9;
}

/* ---- descriptors ---------------------------------------------------------- */

#define EP_IN (xinput ? 0x81 : 0x82)   /* the gamepad's interrupt IN endpoint */
#define EP_OUT 0x01                     /* XInput: LED and rumble commands */
#define EP_CONSUMER 0x81                /* HID mode: the consumer control interface */

static uint16_t vendor_id(void) { return xinput ? 0x045E : 0x0079; }
static uint16_t product_id(void) { return xinput ? 0x028E : 0x0006; }

static int device_descriptor(uint8_t *d) {
    const uint8_t desc[18] = {18, 1, 0x00, 0x02, xinput ? 0xFF : 0x00, xinput ? 0xFF : 0x00, xinput ? 0xFF : 0x00, 8,
                              (uint8_t)vendor_id(), (uint8_t)(vendor_id() >> 8),
                              (uint8_t)product_id(), (uint8_t)(product_id() >> 8),
                              0x14, 0x01, 1, 2, 3, 1};
    memcpy(d, desc, sizeof desc);
    return sizeof desc;
}

static const uint8_t consumer_report_desc[] = {
    0x05, 0x0C, 0x09, 0x01, 0xA1, 0x01, 0x15, 0x00, 0x25, 0x01, 0x09, 0xE9, 0x09, 0xEA,
    0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01, 0x81, 0x01, 0xC0,
};

static const uint8_t pad_report_desc[] = {
    0x05, 0x01, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x35,
    0x15, 0x00, 0x26, 0xFF, 0x00, 0x75, 0x08, 0x95, 0x04, 0x81, 0x02,   /* X Y Z Rz */
    0x09, 0x39, 0x15, 0x00, 0x25, 0x07, 0x75, 0x04, 0x95, 0x01, 0x81, 0x42,  /* hat */
    0x75, 0x04, 0x95, 0x01, 0x81, 0x01,
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0C, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0C, 0x81, 0x02,
    0x75, 0x04, 0x95, 0x01, 0x81, 0x01,
    0x85, 0x02, 0x06, 0x00, 0xFF, 0x09, 0x01, 0x15, 0x00, 0x26, 0xFF, 0x00,
    0x75, 0x08, 0x95, 0x07, 0xB1, 0x02,                                  /* vendor feature report */
    0xC0,
};

static int config_descriptor(uint8_t *d) {
    if (xinput) {
        /* Interface 0: the controller. Interface 1: the headset port, which
         * the host must leave alone. */
        const uint8_t desc[] = {
            9, 2, 0, 0, 2, 1, 0, 0xA0, 250,
            9, 4, 0, 0, 2, 0xFF, 0x5D, 0x01, 0,
            17, 0x21, 0x00, 0x01, 0x01, 0x25, 0x81, 0x14, 0x00, 0x00, 0x00, 0x00, 0x13, 0x01, 0x08, 0x00, 0x00,
            7, 5, 0x81, 3, 32, 0, 4,
            7, 5, 0x01, 3, 32, 0, 8,
            9, 4, 1, 0, 2, 0xFF, 0x5D, 0x03, 0,
            7, 5, 0x82, 3, 32, 0, 2,
            7, 5, 0x02, 3, 32, 0, 4,
        };
        memcpy(d, desc, sizeof desc);
        d[2] = sizeof desc;
        return sizeof desc;
    }
    const uint8_t desc[] = {
        9, 2, 0, 0, 2, 1, 0, 0x80, 50,
        9, 4, 0, 0, 1, 0x03, 0x00, 0x00, 0,
        9, 0x21, 0x10, 0x01, 0, 1, 0x22, sizeof consumer_report_desc, 0,
        7, 5, EP_CONSUMER, 3, 8, 0, 10,
        9, 4, 1, 0, 1, 0x03, 0x00, 0x00, 0,
        9, 0x21, 0x10, 0x01, 0, 1, 0x22, sizeof pad_report_desc, 0,
        7, 5, 0x82, 3, 8, 0, 4,
    };
    memcpy(d, desc, sizeof desc);
    d[2] = sizeof desc;
    return sizeof desc;
}

static int string_descriptor(uint8_t index, uint8_t *d) {
    if (index == 0) {
        const uint8_t langs[4] = {4, 3, 0x09, 0x04};
        memcpy(d, langs, 4);
        return 4;
    }
    const char *s = index == 1 ? (xinput ? "Generic X-Box pad" : "DragonRise Inc.")
                  : index == 2 ? (xinput ? "Controller" : "Generic USB Joystick")
                  : index == 3 ? "AERO0PAD"
                  : NULL;
    if (s == NULL) return -1;
    int n = 2;
    for (; *s; ++s) {
        d[n++] = (uint8_t)*s;
        d[n++] = 0;
    }
    d[0] = (uint8_t)n;
    d[1] = 3;
    return n;
}

/* ---- reports --------------------------------------------------------------- */

static void send_in(const uint8_t *r, int n) {
    struct usb_redir_interrupt_packet_header h = {(uint8_t)EP_IN, usb_redir_success, (uint16_t)n};
    usbredirparser_send_interrupt_packet(parser, 0, &h, (uint8_t *)r, n);
}

static void send_report(void) {
    if (xinput) {
        if (reports_sent == 0) {
            const uint8_t led[3] = {0x01, 0x03, 0x0E};  /* LED status: rotating, not an input report */
            send_in(led, sizeof led);
        }
        /* 00 14, buttons, LT, RT, LX, LY, RX, RY, 6 reserved bytes. */
        const uint16_t buttons = 0x0002 | 0x0008     /* d-pad down, right */
                               | 0x0010              /* Start */
                               | 0x0100              /* LB */
                               | 0x1000 | 0x8000;    /* A, Y */
        const int16_t lx = 32767, ly = 32767, rx = -32768, ry = 0;
        const uint8_t r[20] = {0x00, 0x14, (uint8_t)buttons, (uint8_t)(buttons >> 8), 255, 0,
                               (uint8_t)lx, (uint8_t)(lx >> 8), (uint8_t)ly, (uint8_t)(ly >> 8),
                               (uint8_t)rx, (uint8_t)(rx >> 8), (uint8_t)ry, (uint8_t)(ry >> 8)};
        send_in(r, sizeof r);
    } else {
        /* Report ID 1: X Y Z Rz, hat (6 = left), 12 buttons (2 and 10). */
        const uint16_t buttons = 1u << 1 | 1u << 9;
        const uint8_t r[8] = {0x01, 0, 128, 255, 128, 6, (uint8_t)buttons, (uint8_t)(buttons >> 8)};
        send_in(r, sizeof r);
    }
    if (reports_sent++ == 0) say("first input report sent");
}

/* ---- usbredir callbacks ---------------------------------------------------- */

static void control_packet(void *priv, uint64_t id, struct usb_redir_control_packet_header *c,
                           uint8_t *data, int len) {
    (void)priv;
    (void)len;
    uint8_t reply[512];
    int n = -1;
    struct usb_redir_control_packet_header r = *c;
    const int in = c->requesttype & 0x80;
    if (c->requesttype == 0x80 && c->request == 6) { /* GET_DESCRIPTOR */
        switch (c->value >> 8) {
        case 1: n = device_descriptor(reply); break;
        case 2: n = config_descriptor(reply); break;
        case 3: n = string_descriptor(c->value & 0xFF, reply); break;
        }
    } else if (!xinput && c->requesttype == 0x81 && c->request == 6 && c->value >> 8 == 0x22) { /* report descriptor */
        const uint8_t *desc = c->index == 0 ? consumer_report_desc : pad_report_desc;
        n = c->index == 0 ? (int)sizeof consumer_report_desc : (int)sizeof pad_report_desc;
        memcpy(reply, desc, n);
        say("report descriptor of interface %u read", c->index);
    } else if (c->requesttype == 0x80 && c->request == 0) { /* GET_STATUS */
        reply[0] = reply[1] = 0;
        n = 2;
    } else if (!xinput && c->requesttype == 0x21 && c->request == 0x0A) { /* SET_IDLE */
        n = 0;
    } else if ((c->requesttype & 0x60) == 0 && !in) { /* other standard OUT requests */
        n = 0;
    }
    if (n < 0) {
        say("control %02x/%02x value %04x index %04x: stall", c->requesttype, c->request, c->value, c->index);
        r.status = usb_redir_stall;
        r.length = 0;
        usbredirparser_send_control_packet(parser, id, &r, NULL, 0);
    } else if (in) {
        if (n > c->length) n = c->length;
        r.status = usb_redir_success;
        r.length = (uint16_t)n;
        usbredirparser_send_control_packet(parser, id, &r, reply, n);
    } else {
        r.status = usb_redir_success;
        usbredirparser_send_control_packet(parser, id, &r, NULL, 0);
    }
    if (data) usbredirparser_free_packet_data(parser, data);
}

/* XInput OUT: 01 03 xx sets the LEDs (06 to 09 = player 1 to 4), 00 08 rumble. */
static void interrupt_packet(void *priv, uint64_t id, struct usb_redir_interrupt_packet_header *h,
                             uint8_t *data, int len) {
    (void)priv;
    if (h->endpoint == EP_OUT) {
        if (len == 3 && data[0] == 0x01 && data[1] == 0x03 && data[2] >= 6 && data[2] <= 9)
            say("LED: player %d", data[2] - 5);
        else
            say("OUT %d bytes: %02x %02x %02x", len, len > 0 ? data[0] : 0, len > 1 ? data[1] : 0, len > 2 ? data[2] : 0);
        struct usb_redir_interrupt_packet_header r = *h;
        r.status = usb_redir_success;
        usbredirparser_send_interrupt_packet(parser, id, &r, NULL, 0);
    }
    if (data) usbredirparser_free_packet_data(parser, data);
}

static void set_configuration(void *priv, uint64_t id, struct usb_redir_set_configuration_header *s) {
    (void)priv;
    struct usb_redir_configuration_status_header st = {usb_redir_success, s->configuration};
    usbredirparser_send_configuration_status(parser, id, &st);
}

static void get_configuration(void *priv, uint64_t id) {
    (void)priv;
    struct usb_redir_configuration_status_header st = {usb_redir_success, 1};
    usbredirparser_send_configuration_status(parser, id, &st);
}

static void set_alt_setting(void *priv, uint64_t id, struct usb_redir_set_alt_setting_header *s) {
    (void)priv;
    struct usb_redir_alt_setting_status_header st = {s->alt == 0 ? usb_redir_success : usb_redir_inval, s->interface, 0};
    usbredirparser_send_alt_setting_status(parser, id, &st);
}

static void get_alt_setting(void *priv, uint64_t id, struct usb_redir_get_alt_setting_header *g) {
    (void)priv;
    struct usb_redir_alt_setting_status_header st = {usb_redir_success, g->interface, 0};
    usbredirparser_send_alt_setting_status(parser, id, &st);
}

static void start_interrupt(void *priv, uint64_t id, struct usb_redir_start_interrupt_receiving_header *s) {
    (void)priv;
    struct usb_redir_interrupt_receiving_status_header st = {usb_redir_success, s->endpoint};
    if (s->endpoint == EP_IN) {
        say("host is reading the gamepad endpoint %02x", s->endpoint);
        reports_started = 1;
        next_report = now() + 0.5;
    } else if (!xinput && s->endpoint == EP_CONSUMER) {
        say("FAIL: host reads the consumer control interface as a gamepad");
    }
    usbredirparser_send_interrupt_receiving_status(parser, id, &st);
}

static void stop_interrupt(void *priv, uint64_t id, struct usb_redir_stop_interrupt_receiving_header *s) {
    (void)priv;
    struct usb_redir_interrupt_receiving_status_header st = {usb_redir_success, s->endpoint};
    if (s->endpoint == EP_IN) reports_started = 0;
    usbredirparser_send_interrupt_receiving_status(parser, id, &st);
}

static void reset(void *priv) { (void)priv; }

static void cancel(void *priv, uint64_t id) {
    (void)priv;
    (void)id;
}

static void interface_and_ep_info(void) {
    struct usb_redir_interface_info_header ii = {0};
    struct usb_redir_ep_info_header ep;
    memset(&ep, 0, sizeof ep);
    memset(ep.type, usb_redir_type_invalid, sizeof ep.type);
    ep.type[0] = ep.type[16] = usb_redir_type_control;
    ep.max_packet_size[0] = ep.max_packet_size[16] = 8;
    ii.interface_count = 2;
    for (int i = 0; i < 2; ++i) {
        ii.interface[i] = (uint8_t)i;
        ii.interface_class[i] = xinput ? 0xFF : 0x03;
        ii.interface_subclass[i] = xinput ? 0x5D : 0x00;
        ii.interface_protocol[i] = xinput ? (i == 0 ? 0x01 : 0x03) : 0x00;
    }
    /* Index: IN endpoints at 16 + number, OUT at the number. */
    const struct { int index, mps, interval, iface; } eps[] = {
        {16 + 1, xinput ? 32 : 8, xinput ? 4 : 10, 0},
        {16 + 2, xinput ? 32 : 8, xinput ? 2 : 4, xinput ? 1 : 1},
        {1, 32, 8, 0},
        {2, 32, 4, 1},
    };
    for (int i = 0; i < (xinput ? 4 : 2); ++i) {
        ep.type[eps[i].index] = usb_redir_type_interrupt;
        ep.max_packet_size[eps[i].index] = (uint16_t)eps[i].mps;
        ep.interval[eps[i].index] = (uint8_t)eps[i].interval;
        ep.interface[eps[i].index] = (uint8_t)eps[i].iface;
    }
    usbredirparser_send_interface_info(parser, &ii);
    usbredirparser_send_ep_info(parser, &ep);
}

static void hello(void *priv, struct usb_redir_hello_header *h) {
    (void)priv;
    say("connected to %s", h->version);
    interface_and_ep_info();
    struct usb_redir_device_connect_header dc = {usb_redir_speed_full, xinput ? 0xFF : 0x00, xinput ? 0xFF : 0x00,
                                                 xinput ? 0xFF : 0x00, vendor_id(), product_id(), 0x0114};
    usbredirparser_send_device_connect(parser, &dc);
    say("presenting %04x:%04x", vendor_id(), product_id());
}

static void log_cb(void *priv, int level, const char *msg) {
    (void)priv;
    if (level <= usbredirparser_warning) say("usbredir: %s", msg);
}

static int read_cb(void *priv, uint8_t *data, int count) {
    (void)priv;
    int n = (int)read(sock, data, count);
    if (n == 0) return -1;
    if (n < 0) return errno == EAGAIN ? 0 : -1;
    return n;
}

static int write_cb(void *priv, uint8_t *data, int count) {
    (void)priv;
    int n = (int)write(sock, data, count);
    if (n < 0) return errno == EAGAIN ? 0 : -1;
    return n;
}

int main(int argc, char **argv) {
    if (argc < 3 || (strcmp(argv[2], "--xinput") != 0 && strcmp(argv[2], "--hid") != 0)) {
        fprintf(stderr, "usage: %s <socket> --xinput|--hid\n", argv[0]);
        return 2;
    }
    xinput = strcmp(argv[2], "--xinput") == 0;

    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s", argv[1]);
    unlink(argv[1]);
    if (bind(listener, (struct sockaddr *)&addr, sizeof addr) != 0 || listen(listener, 1) != 0) {
        perror(argv[1]);
        return 1;
    }
    sock = accept(listener, NULL, NULL);
    if (sock < 0) {
        perror("accept");
        return 1;
    }
    fcntl(sock, F_SETFL, fcntl(sock, F_GETFL) | O_NONBLOCK);

    parser = usbredirparser_create();
    parser->log_func = log_cb;
    parser->read_func = read_cb;
    parser->write_func = write_cb;
    parser->hello_func = hello;
    parser->reset_func = reset;
    parser->control_packet_func = control_packet;
    parser->interrupt_packet_func = interrupt_packet;
    parser->set_configuration_func = set_configuration;
    parser->get_configuration_func = get_configuration;
    parser->set_alt_setting_func = set_alt_setting;
    parser->get_alt_setting_func = get_alt_setting;
    parser->start_interrupt_receiving_func = start_interrupt;
    parser->stop_interrupt_receiving_func = stop_interrupt;
    parser->cancel_data_packet_func = cancel;
    uint32_t caps[USB_REDIR_CAPS_SIZE] = {0};
    usbredirparser_caps_set_cap(caps, usb_redir_cap_connect_device_version);
    usbredirparser_caps_set_cap(caps, usb_redir_cap_ep_info_max_packet_size);
    usbredirparser_caps_set_cap(caps, usb_redir_cap_64bits_ids);
    usbredirparser_caps_set_cap(caps, usb_redir_cap_32bits_bulk_length);  /* QEMU wants it on xHCI ports */
    usbredirparser_init(parser, "fakepad 1.0", caps, USB_REDIR_CAPS_SIZE, usbredirparser_fl_usb_host);

    for (;;) {
        struct pollfd p = {sock, POLLIN | (usbredirparser_has_data_to_write(parser) ? POLLOUT : 0), 0};
        poll(&p, 1, 10);
        if (p.revents & (POLLIN | POLLHUP)) {
            if (usbredirparser_do_read(parser) != 0) break;
        }
        if (reports_started && now() >= next_report) {
            send_report();
            next_report = now() + 1.0;
        }
        if (usbredirparser_has_data_to_write(parser) && usbredirparser_do_write(parser) != 0) break;
    }
    say("QEMU went away after %d input report(s)", reports_sent);
    return 0;
}
