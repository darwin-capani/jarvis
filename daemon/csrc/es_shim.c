// es_shim.c — a thin C shim over EndpointSecurity, compiled against Apple's REAL
// <EndpointSecurity/EndpointSecurity.h> so every es_message_t / es_process_t
// struct layout and field name is COMPILER-VERIFIED (not hand-transcribed in
// Rust). It extracts flat scalars + borrowed C strings (valid only for the
// duration of the callback; the Rust side copies them immediately) and calls
// back into Rust. NOTIFY-ONLY: it subscribes to notify events, never auth, so it
// never has to call es_respond and can never block/wedge the subject.
//
// Compiled + linked only under the `endpoint-security` Cargo feature (see
// build.rs). Linking needs no entitlement — es_new_client's entitlement check is
// a RUNTIME gate — so this builds anywhere with the macOS SDK; it only actually
// runs on a device with root + the restricted
// com.apple.developer.endpoint-security.client entitlement + a notarized host.

#include <EndpointSecurity/EndpointSecurity.h>
#include <bsm/libbsm.h>
#include <mach/vm_prot.h>
#include <sys/mman.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>

// The flat ABI handed to Rust. `kind`: 0=mprotect(exec), 1=mmap(MAP_JIT),
// 2=get_task, 3=signal. Paths are borrowed (valid only during the callback).
typedef struct {
    int kind;
    int subject_pid;          // the app the event is ABOUT (introspect keys on this)
    const char *subject_path;
    int actor_pid;            // the acquirer/signaler (get_task/signal); else -1
    const char *actor_path;
    int signal_number;        // signal events only
} darwin_es_event;

typedef void (*darwin_es_callback)(const darwin_es_event *);

static es_client_t *g_client = NULL;
static darwin_es_callback g_cb = NULL;

static const char *proc_path(const es_process_t *p) {
    if (p == NULL || p->executable == NULL || p->executable->path.data == NULL) {
        return "";
    }
    return p->executable->path.data; // es_string_token_t.data is NUL-terminated
}

static int proc_pid(const es_process_t *p) {
    if (p == NULL) {
        return -1;
    }
    // audit_token is a value member; audit_token_to_pid takes it by value (libbsm).
    audit_token_t tok = p->audit_token;
    return (int)audit_token_to_pid(tok);
}

static void handle_message(const es_message_t *msg) {
    if (g_cb == NULL || msg == NULL) {
        return;
    }
    darwin_es_event e;
    memset(&e, 0, sizeof(e));
    e.subject_pid = -1;
    e.actor_pid = -1;
    e.subject_path = "";
    e.actor_path = "";
    e.signal_number = 0;

    switch (msg->event_type) {
        case ES_EVENT_TYPE_NOTIFY_MPROTECT:
            // Only care about making memory executable (the W^X flip toward X).
            if ((msg->event.mprotect.protection & VM_PROT_EXECUTE) == 0) {
                return;
            }
            e.kind = 0;
            e.subject_pid = proc_pid(msg->process);
            e.subject_path = proc_path(msg->process);
            break;

        case ES_EVENT_TYPE_NOTIFY_MMAP:
            // Only care about JIT-eligible executable mappings.
            if ((msg->event.mmap.flags & MAP_JIT) == 0) {
                return;
            }
            e.kind = 1;
            e.subject_pid = proc_pid(msg->process);
            e.subject_path = proc_path(msg->process);
            break;

        case ES_EVENT_TYPE_NOTIFY_GET_TASK:
            // The TARGET is the app whose task port is being acquired; the actor
            // (msg->process) is the acquirer — a debugger/injector attaching.
            e.kind = 2;
            e.subject_pid = proc_pid(msg->event.get_task.target);
            e.subject_path = proc_path(msg->event.get_task.target);
            e.actor_pid = proc_pid(msg->process);
            e.actor_path = proc_path(msg->process);
            break;

        case ES_EVENT_TYPE_NOTIFY_SIGNAL:
            e.kind = 3;
            e.signal_number = msg->event.signal.sig;
            e.subject_pid = proc_pid(msg->event.signal.target);
            e.subject_path = proc_path(msg->event.signal.target);
            e.actor_pid = proc_pid(msg->process);
            e.actor_path = proc_path(msg->process);
            break;

        default:
            return;
    }
    g_cb(&e);
}

// Returns 0 on success; -1 if es_new_client failed (most commonly not entitled /
// not root); -2 if es_subscribe failed.
int darwin_es_start(darwin_es_callback cb) {
    g_cb = cb;
    es_new_client_result_t r = es_new_client(&g_client, ^(es_client_t *c, const es_message_t *msg) {
        (void)c;
        handle_message(msg);
    });
    if (r != ES_NEW_CLIENT_RESULT_SUCCESS) {
        g_client = NULL;
        return -1;
    }
    es_event_type_t events[] = {
        ES_EVENT_TYPE_NOTIFY_MPROTECT,
        ES_EVENT_TYPE_NOTIFY_MMAP,
        ES_EVENT_TYPE_NOTIFY_GET_TASK,
        ES_EVENT_TYPE_NOTIFY_SIGNAL,
    };
    uint32_t count = (uint32_t)(sizeof(events) / sizeof(events[0]));
    if (es_subscribe(g_client, events, count) != ES_RETURN_SUCCESS) {
        es_delete_client(g_client);
        g_client = NULL;
        return -2;
    }
    return 0;
}

void darwin_es_stop(void) {
    if (g_client != NULL) {
        es_unsubscribe_all(g_client);
        es_delete_client(g_client);
        g_client = NULL;
    }
    g_cb = NULL;
}
