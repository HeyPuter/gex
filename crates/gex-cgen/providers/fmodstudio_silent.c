/* PROVIDES fmodstudio.dll i386 stateful
 *
 * SILENT STAND-IN for FMOD Studio, not the audio answer. The user's direction
 * for Noita is FMOD bound to FMOD's own native wasm (HTML5) build, with real
 * audio; this binding is what runs until that build is here, and is meant to
 * be replaced by it (a provider of the same exports over the real library).
 * pewasm's port/src/noita_fmod_silent.c, in the F_body form.
 *
 * Why: W3 has no Wine audio driver (no winepulse/winealsa/wineoss), so
 * mmdevapi has no endpoint and FMOD runs its no-sound output, which still
 * mixes in real time: the real libraries' mixer, Studio update and stream
 * threads cost ~8 ms of CPU a world frame (edger, master 28dafd57: 5.5 +
 * 1.6 + 1.0) for audio nobody hears. Here every export Noita imports from
 * fmodstudio.dll succeeds silently, and System::create/initialize never
 * reach the real library, so none of FMOD's threads ever starts (fmod.dll's
 * own exports are fmod_silent.c's).
 *
 * Objects are real guest blocks from this provider's static pools, so the
 * game's validity checks and user data behave: [0] kind, [4] user data,
 * [8] the guest address of the path (descriptions; interned by path, so a
 * getEvent of the same path gives the same description). Every export is a
 * __stdcall member (`this` the first stack argument; System::create is a
 * static __stdcall): the callee pops `this` and the arguments. */

#define FMOD_OK 0u
#define FMOD_ERR_EVENT_NOTFOUND 74u

enum { SYSTEM = 0x53595331u, CORE, BANK, BUS, DESCRIPTION, INSTANCE };

/* ---- the pools (one lock: the game calls Studio from its main thread) ---- */

static int lock_word;
static void lock(void) {
    int z = 0;
    while (!__atomic_compare_exchange_n(&lock_word, &z, 1, 0, __ATOMIC_ACQUIRE, __ATOMIC_RELAXED)) {
        z = 0;
    }
}
static void unlock(void) { __atomic_store_n(&lock_word, 0, __ATOMIC_RELEASE); }

typedef struct { uint32_t kind, user, path; } object_t;

/* Instances come and go (one per sound played): a free list. */
#define N_INSTANCES 32768u
static object_t instances[N_INSTANCES];
static uint32_t instance_free = 0, instance_next = 0; /* free list through .user, 1-based */
/* Systems, banks, buses, descriptions and the master group: never released
 * by the game but per name, so interned (descriptions, buses, banks). */
#define N_NAMED 16384u
static object_t named[N_NAMED];
static uint32_t named_next = 0;
#define ARENA 1048576u
static char arena[ARENA];
static uint32_t arena_next = 0;
#define HASH 32768u
static uint32_t hash_slot[HASH]; /* index+1 into named[] */

static uint32_t guest(const void *p) { return (uint32_t)(uintptr_t)p; }

static uint32_t new_instance(void) {
    lock();
    uint32_t i;
    if (instance_free) {
        i = instance_free - 1;
        instance_free = instances[i].user;
    } else if (instance_next < N_INSTANCES) {
        i = instance_next++;
    } else {
        unlock();
        return 0;
    }
    instances[i].kind = INSTANCE;
    instances[i].user = 0;
    instances[i].path = 0;
    unlock();
    return guest(&instances[i]);
}
static void free_instance(uint32_t o) {
    uint32_t base = guest(&instances[0]);
    if (o < base || o >= base + sizeof(instances) || (o - base) % sizeof(object_t)) {
        return;
    }
    uint32_t i = (o - base) / sizeof(object_t);
    lock();
    if (instances[i].kind == INSTANCE) {
        instances[i].kind = 0;
        instances[i].user = instance_free;
        instance_free = i + 1;
    }
    unlock();
}

static uint32_t strlen_g(uint32_t s) {
    uint32_t n = 0;
    while (*(gex_u8 *)GEX_P(s + n)) {
        ++n;
    }
    return n;
}
/* The object of `kind` named by the guest string at `s` (NULL: unnamed),
 * the same one for the same name. 0 when the pools are full. */
static uint32_t named_object(uint32_t kind, uint32_t s) {
    uint32_t n = s ? strlen_g(s) : 0, h = kind * 2654435761u;
    for (uint32_t k = 0; k < n; ++k) {
        h = (h ^ *(gex_u8 *)GEX_P(s + k)) * 16777619u;
    }
    lock();
    uint32_t slot = h % HASH;
    for (uint32_t probe = 0; s && probe < HASH; ++probe, slot = (slot + 1) % HASH) {
        uint32_t j = hash_slot[slot];
        if (!j) {
            break;
        }
        object_t *o = &named[j - 1];
        if (o->kind != kind || !o->path) {
            continue;
        }
        const char *a = &arena[o->path - guest(arena)];
        uint32_t k = 0;
        while (k < n && a[k] == (char)*(gex_u8 *)GEX_P(s + k)) {
            ++k;
        }
        if (k == n && !a[n]) {
            unlock();
            return guest(o);
        }
    }
    if (named_next >= N_NAMED || (s && arena_next + n + 1 > ARENA)) {
        unlock();
        return 0;
    }
    object_t *o = &named[named_next++];
    o->kind = kind;
    o->user = 0;
    o->path = 0;
    if (s) {
        char *a = &arena[arena_next];
        for (uint32_t k = 0; k < n; ++k) {
            a[k] = (char)*(gex_u8 *)GEX_P(s + k);
        }
        a[n] = 0;
        arena_next += n + 1;
        o->path = guest(a);
        if (!hash_slot[slot]) {
            hash_slot[slot] = named_next;
        }
    }
    unlock();
    return guest(o);
}

static void out32(uint32_t a, uint32_t v) {
    if (a) {
        GW(a) = v;
    }
}
static uint32_t kind_of(uint32_t o) { return o ? GW(o) : 0; }

/* FMOD_OK, popping `this` and `args` stack words. */
#define OK(nargs) return gp_return_eax(ctx, ret, 4u * (nargs), FMOD_OK)
#define BODY(name) static GEX_PROVIDER(name)
#define NOOP(name, nargs) \
    BODY(name) {          \
        (void)depth;      \
        (void)need;       \
        OK(nargs);        \
    }

static uint32_t studio_system, core_system;

/* ---- FMOD::Studio::System ---- */

/* static System::create(System **system, unsigned headerversion) */
BODY(sys_create) {
    (void)depth, (void)need;
    if (!studio_system) {
        studio_system = named_object(SYSTEM, 0);
        core_system = named_object(CORE, 0);
    }
    out32(gp_arg(ctx, 0), studio_system);
    OK(2);
}
BODY(sys_is_valid) {
    (void)depth, (void)need;
    uint32_t t = gp_arg(ctx, 0);
    return gp_return_eax(ctx, ret, 4, t && t == studio_system);
}
NOOP(sys_initialize, 5)
NOOP(sys_release, 1)
NOOP(sys_update, 1)
NOOP(sys_set_callback, 3)
NOOP(sys_set_listener, 4)
BODY(sys_get_core) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), core_system);
    OK(2);
}
/* loadBankFile(const char *filename, unsigned flags, Bank **bank) */
BODY(sys_load_bank) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 3), named_object(BANK, gp_arg(ctx, 1)));
    OK(4);
}
/* getBus(const char *path, Bus **bus) */
BODY(sys_get_bus) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 2), named_object(BUS, gp_arg(ctx, 1)));
    OK(3);
}
/* getEvent(const char *path, EventDescription **event) */
BODY(sys_get_event) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 2), named_object(DESCRIPTION, gp_arg(ctx, 1)));
    OK(3);
}

/* ---- Bank / Bus ---- */

BODY(bank_event_count) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), 0);
    OK(2);
}
/* getEventList(EventDescription **array, int capacity, int *count) */
BODY(bank_event_list) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 3), 0);
    OK(4);
}
NOOP(bus_set_volume, 2)
NOOP(bus_set_paused, 2)

/* ---- EventDescription ---- */

NOOP(desc_load_samples, 1)
BODY(desc_create_instance) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), new_instance());
    OK(2);
}
BODY(desc_is_snapshot) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 1);
    if (p) {
        *(gex_u8 *)GEX_P(p) = 0;
    }
    OK(2);
}
BODY(desc_max_distance) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), 0x42c80000u); /* 100.0f */
    OK(2);
}
BODY(desc_min_distance) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), 0x3f800000u); /* 1.0f */
    OK(2);
}
BODY(desc_length) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), 0);
    OK(2);
}
/* getUserProperty(const char *name, FMOD_STUDIO_USER_PROPERTY *property) */
BODY(desc_user_property) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 12, FMOD_ERR_EVENT_NOTFOUND);
}
/* getPath(char *path, int size, int *retrieved) */
BODY(desc_path) {
    (void)depth, (void)need;
    uint32_t t = gp_arg(ctx, 0), out = gp_arg(ctx, 1), size = gp_arg(ctx, 2);
    uint32_t path = kind_of(t) == DESCRIPTION ? GW(t + 8) : 0;
    uint32_t n = (path ? strlen_g(path) : 0) + 1;
    out32(gp_arg(ctx, 3), n);
    if (out && (int32_t)size > 0) {
        uint32_t m = n < size ? n : size;
        for (uint32_t k = 0; k + 1 < m; ++k) {
            *(gex_u8 *)GEX_P(out + k) = *(gex_u8 *)GEX_P(path + k);
        }
        *(gex_u8 *)GEX_P(out + m - 1) = 0;
    }
    OK(4);
}

/* ---- EventInstance ---- */

BODY(inst_is_valid) {
    (void)depth, (void)need;
    return gp_return_eax(ctx, ret, 4, kind_of(gp_arg(ctx, 0)) == INSTANCE);
}
NOOP(inst_start, 1)
NOOP(inst_stop, 2)
BODY(inst_release) {
    (void)depth, (void)need;
    free_instance(gp_arg(ctx, 0));
    OK(1);
}
NOOP(inst_set_volume, 2)
NOOP(inst_set_paused, 2)
NOOP(inst_set_3d, 2)
NOOP(inst_set_param, 4)
NOOP(inst_set_callback, 3)
BODY(inst_set_user) {
    (void)depth, (void)need;
    uint32_t t = gp_arg(ctx, 0);
    if (kind_of(t) == INSTANCE) {
        GW(t + 4) = gp_arg(ctx, 1);
    }
    OK(2);
}
BODY(inst_get_user) {
    (void)depth, (void)need;
    uint32_t t = gp_arg(ctx, 0);
    out32(gp_arg(ctx, 1), kind_of(t) == INSTANCE ? GW(t + 4) : 0);
    OK(2);
}
BODY(inst_timeline) {
    (void)depth, (void)need;
    out32(gp_arg(ctx, 1), 0);
    OK(2);
}

GEX_PROVIDE(sys_create, "?create@System@Studio@FMOD@@SG?AW4FMOD_RESULT@@PAPAV123@I@Z")
GEX_PROVIDE(sys_is_valid, "?isValid@System@Studio@FMOD@@QBG_NXZ")
GEX_PROVIDE(sys_initialize, "?initialize@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@HIIPAX@Z")
GEX_PROVIDE(sys_release, "?release@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@XZ")
GEX_PROVIDE(sys_update, "?update@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@XZ")
GEX_PROVIDE(sys_set_callback, "?setCallback@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@P6G?AW44@PAUFMOD_STUDIO_SYSTEM@@IPAX1@ZI@Z")
GEX_PROVIDE(sys_set_listener, "?setListenerAttributes@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@HPBUFMOD_3D_ATTRIBUTES@@PBUFMOD_VECTOR@@@Z")
GEX_PROVIDE(sys_get_core, "?getCoreSystem@System@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAPAV13@@Z")
GEX_PROVIDE(sys_load_bank, "?loadBankFile@System@Studio@FMOD@@QAG?AW4FMOD_RESULT@@PBDIPAPAVBank@23@@Z")
GEX_PROVIDE(sys_get_bus, "?getBus@System@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PBDPAPAVBus@23@@Z")
GEX_PROVIDE(sys_get_event, "?getEvent@System@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PBDPAPAVEventDescription@23@@Z")
GEX_PROVIDE(bank_event_count, "?getEventCount@Bank@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAH@Z")
GEX_PROVIDE(bank_event_list, "?getEventList@Bank@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAPAVEventDescription@23@HPAH@Z")
GEX_PROVIDE(bus_set_volume, "?setVolume@Bus@Studio@FMOD@@QAG?AW4FMOD_RESULT@@M@Z")
GEX_PROVIDE(bus_set_paused, "?setPaused@Bus@Studio@FMOD@@QAG?AW4FMOD_RESULT@@_N@Z")
GEX_PROVIDE(desc_load_samples, "?loadSampleData@EventDescription@Studio@FMOD@@QAG?AW4FMOD_RESULT@@XZ")
GEX_PROVIDE(desc_create_instance, "?createInstance@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAPAVEventInstance@23@@Z")
GEX_PROVIDE(desc_is_snapshot, "?isSnapshot@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PA_N@Z")
GEX_PROVIDE(desc_max_distance, "?getMaximumDistance@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAM@Z")
GEX_PROVIDE(desc_min_distance, "?getMinimumDistance@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAM@Z")
GEX_PROVIDE(desc_length, "?getLength@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAH@Z")
GEX_PROVIDE(desc_user_property, "?getUserProperty@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PBDPAUFMOD_STUDIO_USER_PROPERTY@@@Z")
GEX_PROVIDE(desc_path, "?getPath@EventDescription@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PADHPAH@Z")
GEX_PROVIDE(inst_is_valid, "?isValid@EventInstance@Studio@FMOD@@QBG_NXZ")
GEX_PROVIDE(inst_start, "?start@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@XZ")
GEX_PROVIDE(inst_stop, "?stop@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@W4FMOD_STUDIO_STOP_MODE@@@Z")
GEX_PROVIDE(inst_release, "?release@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@XZ")
GEX_PROVIDE(inst_set_volume, "?setVolume@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@M@Z")
GEX_PROVIDE(inst_set_paused, "?setPaused@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@_N@Z")
GEX_PROVIDE(inst_set_3d, "?set3DAttributes@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@PBUFMOD_3D_ATTRIBUTES@@@Z")
GEX_PROVIDE(inst_set_param, "?setParameterByName@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@PBDM_N@Z")
GEX_PROVIDE(inst_set_callback, "?setCallback@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@P6G?AW44@IPAUFMOD_STUDIO_EVENTINSTANCE@@PAX@ZI@Z")
GEX_PROVIDE(inst_set_user, "?setUserData@EventInstance@Studio@FMOD@@QAG?AW4FMOD_RESULT@@PAX@Z")
GEX_PROVIDE(inst_get_user, "?getUserData@EventInstance@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAPAX@Z")
GEX_PROVIDE(inst_timeline, "?getTimelinePosition@EventInstance@Studio@FMOD@@QBG?AW4FMOD_RESULT@@PAH@Z")
