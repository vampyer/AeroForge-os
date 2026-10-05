/* fakebt: a simulated USB Bluetooth adapter for testing AeroForge in QEMU,
 * which has no Bluetooth device of its own.
 *
 * It speaks the usbredir protocol (the "USB host" side, like usbredirserver)
 * on a Unix socket; QEMU attaches it to the guest's xHCI controller with
 *   -chardev socket,id=fakebt,path=build/fakebt.sock -device usb-redir,chardev=fakebt,bus=xhci.0,port=3
 *
 * In MediaTek mode (the default) it looks like an MT7921 adapter
 * (0e8d:0608): until the guest downloads the firmware patch over the WMT
 * vendor protocol and switches Bluetooth on, it refuses ordinary HCI
 * commands. Every downloaded byte is compared with the firmware file, and the
 * result is logged ("firmware OK ..."). With --generic it is a plain adapter
 * (0a12:0001) that needs no firmware.
 *
 * Once up it answers the HCI commands the AeroForge host sends and, during a
 * scan, reports a classic gamepad, a classic headset and an LE gamepad.
 *
 * Build: cc -O2 -o build/fakebt tools/fakebt/fakebt.c -lusbredirparser
 * Run:   build/fakebt <socket> <firmware file> [--generic] */

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>
#include <usbredirparser.h>

#define EP_EVENT 0x81
#define EP_ACL_IN 0x82
#define EP_ACL_OUT 0x02
#define EVENT_MPS 16
#define BULK_MPS 512

static struct usbredirparser *parser;
static int sock = -1;
static int generic;

/* ---- logging -------------------------------------------------------------- */

static void say(const char *fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "fakebt: ");
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

static uint16_t vendor_id(void) { return generic ? 0x0A12 : 0x0E8D; }
static uint16_t product_id(void) { return generic ? 0x0001 : 0x0608; }

static int device_descriptor(uint8_t *d) {
    const uint8_t desc[18] = {18, 1, 0x00, 0x02, 0xE0, 0x01, 0x01, 64,
                              (uint8_t)vendor_id(), (uint8_t)(vendor_id() >> 8),
                              (uint8_t)product_id(), (uint8_t)(product_id() >> 8),
                              0x00, 0x01, 1, 2, 0, 1};
    memcpy(d, desc, sizeof desc);
    return sizeof desc;
}

/* Interface 0: HCI events and ACL data. Interface 1: SCO audio (isochronous,
 * alternate setting 0 has no bandwidth), present as on real adapters. */
static int config_descriptor(uint8_t *d) {
    const uint8_t desc[] = {
        9, 2, 0, 0, 2, 1, 0, 0xE0, 50,
        9, 4, 0, 0, 3, 0xE0, 0x01, 0x01, 0,
        7, 5, EP_EVENT, 3, EVENT_MPS, 0, 1,
        7, 5, EP_ACL_IN, 2, BULK_MPS & 0xFF, BULK_MPS >> 8, 0,
        7, 5, EP_ACL_OUT, 2, BULK_MPS & 0xFF, BULK_MPS >> 8, 0,
        9, 4, 1, 0, 2, 0xE0, 0x01, 0x01, 0,
        7, 5, 0x83, 1, 0, 0, 1,
        7, 5, 0x03, 1, 0, 0, 1,
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
    const char *s = index == 1 ? (generic ? "Generic" : "MediaTek Inc.")
                  : index == 2 ? (generic ? "Bluetooth Radio" : "Wireless_Device")
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

/* ---- MediaTek firmware download ------------------------------------------ */

static uint8_t *fw;
static size_t fw_size;
static int fw_section = -1;       /* section being downloaded */
static uint32_t fw_offset, fw_left, fw_bytes, fw_sections, fw_sections_total;
static int fw_mismatch;
static int bt_on;                 /* Bluetooth function switched on */
static uint32_t ep_reset_opt;

static uint8_t wmt_event[64];
static int wmt_event_len;

static uint32_t le32(const uint8_t *p) {
    return p[0] | p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

static void wmt_reply(uint8_t op, uint8_t flag, const uint8_t *extra, int n) {
    uint8_t *e = wmt_event;
    e[0] = 0xE4;
    e[1] = (uint8_t)(5 + n);
    e[2] = 2;  /* chip to host */
    e[3] = op;
    e[4] = 0;
    e[5] = (uint8_t)(1 + n);
    e[6] = flag;
    memcpy(e + 7, extra, n);
    wmt_event_len = 7 + n;
}

/* The section the guest just announced (52 bytes of its section map). */
static int find_section(const uint8_t *announce) {
    if (fw_size < 96) return -1;
    uint32_t count = le32(fw + 32 + 12);
    for (uint32_t i = 0; i < count && 96 + 64 * (i + 1) <= fw_size; ++i) {
        const uint8_t *map = fw + 96 + 64 * i;
        if (memcmp(map + 12, announce, 52) == 0) return (int)i;
    }
    return -1;
}

static void wmt_command(const uint8_t *p, int len) {
    if (len < 5 || p[0] != 1) {
        say("malformed WMT command");
        return;
    }
    const uint8_t op = p[1], flag = p[4];
    const uint8_t *data = p + 5;
    const int dlen = len - 5;
    if (op == 0x01) {  /* patch download */
        if (flag == 0) {
            if (fw_sections_total == 0 && fw_size >= 96) {
                /* Sections with nothing to download are never announced. */
                for (uint32_t i = 0, n = le32(fw + 32 + 12); i < n && 96 + 64 * (i + 1) <= fw_size; ++i)
                    if (le32(fw + 96 + 64 * i + 16) != 0) ++fw_sections_total;
            }
            if (dlen != 53 || (fw_section = find_section(data + 1)) < 0) {
                say("FAIL: announced firmware section is not in the firmware file");
                fw_mismatch = 1;
                fw_section = -1;
            } else {
                const uint8_t *map = fw + 96 + 64 * fw_section;
                fw_offset = le32(map + 4);
                fw_left = le32(map + 16);
            }
            wmt_reply(op, 0, NULL, 0);  /* "not downloaded yet, send it" */
            return;
        }
        if (fw_section < 0 || (uint32_t)dlen > fw_left || fw_offset + dlen > fw_size ||
            memcmp(fw + fw_offset, data, dlen) != 0) {
            if (!fw_mismatch) say("FAIL: firmware bytes differ from the file at offset %u", fw_offset);
            fw_mismatch = 1;
        } else {
            fw_offset += dlen;
            fw_left -= dlen;
            fw_bytes += dlen;
            if (flag == 3) {
                if (fw_left != 0) {
                    say("FAIL: section %d ended %u bytes early", fw_section, fw_left);
                    fw_mismatch = 1;
                }
                ++fw_sections;
                fw_section = -1;
            }
        }
        wmt_reply(op, 0, NULL, 0);
    } else if (op == 0x06) {  /* function control: Bluetooth on */
        const uint8_t done[2] = {0x04, 0x04};
        if (dlen == 1 && data[0] == 1) {
            if (fw_mismatch || fw_sections == 0 || fw_sections != fw_sections_total) {
                say("FAIL: Bluetooth switched on without a complete firmware (%u of %u sections)",
                    fw_sections, fw_sections_total);
            } else {
                say("firmware OK: %u section(s), %u bytes, identical to the file", fw_sections, fw_bytes);
            }
            if (ep_reset_opt != 0x00010001) say("FAIL: endpoint reset option not written before switching on");
            bt_on = 1;
            say("Bluetooth function on");
        }
        wmt_reply(op, 0, done, 2);
    } else {
        say("WMT op %#x ignored", op);
        wmt_reply(op, 0, NULL, 0);
    }
}

/* ---- HCI ------------------------------------------------------------------ */

#define MAX_EVENTS 64
struct pending { double at; uint8_t data[260]; int len; };
static struct pending events[MAX_EVENTS];
static int event_count;
static int interrupt_started;
static double inquiry_end;
static int le_scanning;
static double le_report_at;
static int commands;

static void queue_event(double delay, const uint8_t *e, int len) {
    if (event_count == MAX_EVENTS) return;
    events[event_count].at = now() + delay;
    memcpy(events[event_count].data, e, len);
    events[event_count].len = len;
    ++event_count;
}

static void command_complete(uint16_t opcode, const uint8_t *ret, int n) {
    uint8_t e[260] = {0x0E, (uint8_t)(3 + n), 1, (uint8_t)opcode, (uint8_t)(opcode >> 8)};
    memcpy(e + 5, ret, n);
    queue_event(0, e, 5 + n);
}

static void command_status(uint16_t opcode, uint8_t status) {
    const uint8_t e[] = {0x0F, 4, status, 1, (uint8_t)opcode, (uint8_t)(opcode >> 8)};
    queue_event(0, e, sizeof e);
}

static void scan_results(void) {
    /* Classic gamepad: extended inquiry result, class 0x002508 (peripheral,
     * gamepad), EIR with a name and the HID service. */
    uint8_t e[2 + 255] = {0x2F, 255, 1, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 1, 0, 0x08, 0x25, 0x00, 0x34, 0x12, (uint8_t)-48};
    const char name[] = "Wireless Gamepad";
    int n = 17;
    e[n++] = (uint8_t)(sizeof name);
    e[n++] = 0x09;
    memcpy(e + n, name, sizeof name - 1);
    n += sizeof name - 1;
    const uint8_t uuids[] = {3, 0x03, 0x24, 0x11};
    memcpy(e + n, uuids, sizeof uuids);
    queue_event(0.3, e, 2 + 255);

    /* Classic headset with a microphone: inquiry result with RSSI, class
     * 0x240404 (audio, headset). */
    const uint8_t h[] = {0x22, 15, 1, 0x01, 0xEF, 0xBE, 0xAD, 0xDE, 0x00, 1, 0, 0x04, 0x04, 0x24, 0x78, 0x56, (uint8_t)-71};
    queue_event(0.6, h, sizeof h);
}

static void le_report(void) {
    /* LE gamepad advertising with a random address: flags, appearance 0x03C4
     * (gamepad), the HID service (0x1812) and a name. */
    const uint8_t ad[] = {2, 0x01, 0x06, 3, 0x19, 0xC4, 0x03, 3, 0x03, 0x12, 0x18,
                          8, 0x09, 'B', 'L', 'E', ' ', 'P', 'a', 'd'};
    uint8_t e[64] = {0x3E, 0, 0x02, 1, 0x00, 0x01, 0x34, 0x12, 0x00, 0xEE, 0xFF, 0xC0, sizeof ad};
    memcpy(e + 13, ad, sizeof ad);
    e[13 + sizeof ad] = (uint8_t)-60;
    e[1] = (uint8_t)(12 + sizeof ad);
    queue_event(0, e, 14 + sizeof ad);
}

static void hci_command(const uint8_t *p, int len) {
    if (len < 3 || len != 3 + p[2]) {
        say("malformed HCI command (%d bytes)", len);
        return;
    }
    const uint16_t op = p[0] | p[1] << 8;
    const uint8_t *a = p + 3;
    if (op == 0xFC6F) {
        wmt_command(a, p[2]);
        return;
    }
    ++commands;
    if (!generic && !bt_on) {
        say("FAIL: HCI command %04x before the firmware was loaded", op);
        const uint8_t disallowed = 0x0C;
        command_complete(op, &disallowed, 1);
        return;
    }
    const uint8_t ok = 0;
    switch (op) {
    case 0x0C03: /* Reset */
        inquiry_end = 0;
        le_scanning = 0;
        say("HCI reset");
        command_complete(op, &ok, 1);
        break;
    case 0x1001: { /* Read Local Version */
        const uint16_t maker = generic ? 10 : 70;
        const uint8_t r[] = {0, 11, 0x00, 0x00, 11, (uint8_t)maker, (uint8_t)(maker >> 8), 0x01, 0x22};
        command_complete(op, r, sizeof r);
        break;
    }
    case 0x1003: { /* Read Local Supported Features */
        const uint8_t r[] = {0, 0xFF, 0xFE, 0x8F, 0xFE, 0xD8, 0x3F, 0x5B, 0x87};
        command_complete(op, r, sizeof r);
        break;
    }
    case 0x1005: { /* Read Buffer Size */
        const uint8_t r[] = {0, 0xFD, 0x03, 0x40, 8, 0, 0, 0};
        command_complete(op, r, sizeof r);
        break;
    }
    case 0x1009: { /* Read BD_ADDR: F0:0D:AE:F0:12:01 */
        const uint8_t r[] = {0, 0x01, 0x12, 0xF0, 0xAE, 0x0D, 0xF0};
        command_complete(op, r, sizeof r);
        break;
    }
    case 0x2002: { /* LE Read Buffer Size */
        const uint8_t r[] = {0, 0xFB, 0x00, 8};
        command_complete(op, r, sizeof r);
        break;
    }
    case 0x0C13: /* Write Local Name */
        say("local name set to \"%.*s\"", (int)strnlen((const char *)a, p[2]), a);
        command_complete(op, &ok, 1);
        break;
    case 0x0C45: /* Write Inquiry Mode */
        if (a[0] != 2) say("inquiry mode %u (expected 2, extended results)", a[0]);
        command_complete(op, &ok, 1);
        break;
    case 0x0C01: case 0x2001: case 0x200B:
        command_complete(op, &ok, 1);
        break;
    case 0x200C: /* LE Set Scan Enable */
        le_scanning = a[0];
        if (le_scanning) le_report_at = now() + 0.4;
        say("LE scan %s", le_scanning ? "on" : "off");
        command_complete(op, &ok, 1);
        break;
    case 0x0401: { /* Inquiry */
        const uint32_t lap = a[0] | a[1] << 8 | (uint32_t)a[2] << 16;
        say("inquiry, LAP %06x, %u x 1.28 s", lap, a[3]);
        command_status(op, 0);
        scan_results();
        inquiry_end = now() + a[3] * 1.0;
        break;
    }
    case 0x0402: /* Inquiry Cancel */
        inquiry_end = 0;
        command_complete(op, &ok, 1);
        break;
    default: {
        say("unknown HCI command %04x", op);
        const uint8_t unknown = 0x01;
        command_complete(op, &unknown, 1);
    }
    }
}

/* Events go out on the interrupt endpoint in max-packet-size fragments. */
static void send_due_events(void) {
    const double t = now();
    if (inquiry_end && t >= inquiry_end) {
        inquiry_end = 0;
        const uint8_t e[] = {0x01, 1, 0};
        queue_event(0, e, sizeof e);
        say("inquiry complete");
    }
    if (le_scanning && le_report_at && t >= le_report_at) {
        le_report_at = 0;
        le_report();
    }
    if (!interrupt_started) return;
    int i = 0;
    while (i < event_count) {
        if (events[i].at > t) {
            ++i;
            continue;
        }
        for (int off = 0; off < events[i].len; off += EVENT_MPS) {
            int n = events[i].len - off < EVENT_MPS ? events[i].len - off : EVENT_MPS;
            struct usb_redir_interrupt_packet_header h = {EP_EVENT, usb_redir_success, (uint16_t)n};
            usbredirparser_send_interrupt_packet(parser, 0, &h, events[i].data + off, n);
        }
        memmove(events + i, events + i + 1, (event_count - i - 1) * sizeof events[0]);
        --event_count;
    }
}

/* ---- usbredir callbacks ---------------------------------------------------- */

static void control_packet(void *priv, uint64_t id, struct usb_redir_control_packet_header *c,
                           uint8_t *data, int len) {
    (void)priv;
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
    } else if (c->requesttype == 0x80 && c->request == 0) { /* GET_STATUS */
        reply[0] = reply[1] = 0;
        n = 2;
    } else if (c->requesttype == 0x20 && c->request == 0) { /* HCI command */
        hci_command(data, len);
        n = len;
    } else if (!generic && c->requesttype == 0xC0 && c->request == 0x63 && c->length == 4) { /* register read */
        const uint32_t reg = (uint32_t)c->value << 16 | c->index;
        uint32_t v = 0;
        if (reg == 0x70010200) v = 0x7961;   /* chip id */
        if (reg == 0x80021004) v = 0x0001;   /* firmware version: file _1_2 */
        if (reg == 0x70010020) v = 0;        /* flavor bit clear */
        for (int i = 0; i < 4; ++i) reply[i] = (uint8_t)(v >> (8 * i));
        n = 4;
    } else if (!generic && c->requesttype == 0x5E && c->request == 0x02 && len == 4) { /* UHW write */
        const uint32_t reg = (uint32_t)c->value << 16 | c->index;
        if (reg == 0x74011890) ep_reset_opt = le32(data);
        n = 4;
    } else if (!generic && c->requesttype == 0xC0 && c->request == 0x01 && c->value == 48) { /* WMT event */
        n = wmt_event_len < c->length ? wmt_event_len : c->length;
        memcpy(reply, wmt_event, n);
        wmt_event_len = 0;
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

static void bulk_packet(void *priv, uint64_t id, struct usb_redir_bulk_packet_header *b,
                        uint8_t *data, int len) {
    (void)priv;
    struct usb_redir_bulk_packet_header r = *b;
    if (b->endpoint == EP_ACL_OUT) {
        say("ACL data, %d bytes", len);
        r.status = usb_redir_success;
        usbredirparser_send_bulk_packet(parser, id, &r, NULL, 0);
    }
    /* ACL IN requests stay pending: nothing is connected, so no data. */
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
    if (s->endpoint == EP_EVENT) interrupt_started = 1;
    else st.status = usb_redir_inval;
    usbredirparser_send_interrupt_receiving_status(parser, id, &st);
}

static void stop_interrupt(void *priv, uint64_t id, struct usb_redir_stop_interrupt_receiving_header *s) {
    (void)priv;
    struct usb_redir_interrupt_receiving_status_header st = {usb_redir_success, s->endpoint};
    if (s->endpoint == EP_EVENT) interrupt_started = 0;
    usbredirparser_send_interrupt_receiving_status(parser, id, &st);
}

static void reset(void *priv) {
    (void)priv;
    say("USB reset");
}

static void cancel(void *priv, uint64_t id) {
    (void)priv;
    (void)id;
}

static void hello(void *priv, struct usb_redir_hello_header *h) {
    (void)priv;
    say("connected to %s", h->version);
    struct usb_redir_interface_info_header ii = {0};
    ii.interface_count = 2;
    for (int i = 0; i < 2; ++i) {
        ii.interface[i] = (uint8_t)i;
        ii.interface_class[i] = 0xE0;
        ii.interface_subclass[i] = 0x01;
        ii.interface_protocol[i] = 0x01;
    }
    usbredirparser_send_interface_info(parser, &ii);

    struct usb_redir_ep_info_header ep;
    memset(&ep, 0, sizeof ep);
    memset(ep.type, usb_redir_type_invalid, sizeof ep.type);
    ep.type[0] = ep.type[16] = usb_redir_type_control;
    ep.max_packet_size[0] = ep.max_packet_size[16] = 64;
    ep.type[16 + 1] = usb_redir_type_interrupt;   /* 0x81 */
    ep.interval[16 + 1] = 1;
    ep.max_packet_size[16 + 1] = EVENT_MPS;
    ep.type[16 + 2] = usb_redir_type_bulk;        /* 0x82 */
    ep.max_packet_size[16 + 2] = BULK_MPS;
    ep.type[2] = usb_redir_type_bulk;             /* 0x02 */
    ep.max_packet_size[2] = BULK_MPS;
    usbredirparser_send_ep_info(parser, &ep);

    struct usb_redir_device_connect_header dc = {usb_redir_speed_high, 0xE0, 0x01, 0x01,
                                                 vendor_id(), product_id(), 0x0100};
    usbredirparser_send_device_connect(parser, &dc);
    say("presenting %s adapter %04x:%04x", generic ? "a generic" : "an MT7921", vendor_id(), product_id());
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

/* ---- main ------------------------------------------------------------------ */

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <socket> <firmware file> [--generic]\n", argv[0]);
        return 2;
    }
    generic = argc > 3 && strcmp(argv[3], "--generic") == 0;
    if (!generic) {
        FILE *f = fopen(argv[2], "rb");
        if (!f) {
            perror(argv[2]);
            return 1;
        }
        fseek(f, 0, SEEK_END);
        fw_size = (size_t)ftell(f);
        rewind(f);
        fw = malloc(fw_size);
        if (!fw || fread(fw, 1, fw_size, f) != fw_size) return 1;
        fclose(f);
    }

    int listener = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    snprintf(addr.sun_path, sizeof addr.sun_path, "%s", argv[1]);
    unlink(argv[1]);
    if (bind(listener, (struct sockaddr *)&addr, sizeof addr) != 0 || listen(listener, 1) != 0) {
        perror(argv[1]);
        return 1;
    }
    say("waiting for QEMU on %s", argv[1]);
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
    parser->bulk_packet_func = bulk_packet;
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
    usbredirparser_caps_set_cap(caps, usb_redir_cap_32bits_bulk_length);
    usbredirparser_init(parser, "fakebt 1.0", caps, USB_REDIR_CAPS_SIZE, usbredirparser_fl_usb_host);

    for (;;) {
        struct pollfd p = {sock, POLLIN | (usbredirparser_has_data_to_write(parser) ? POLLOUT : 0), 0};
        poll(&p, 1, 10);
        if (p.revents & (POLLIN | POLLHUP)) {
            if (usbredirparser_do_read(parser) != 0) break;
        }
        send_due_events();
        if (usbredirparser_has_data_to_write(parser) && usbredirparser_do_write(parser) != 0) break;
    }
    say("QEMU went away after %d HCI command(s)", commands);
    return 0;
}
