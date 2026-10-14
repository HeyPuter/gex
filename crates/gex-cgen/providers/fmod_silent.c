/* PROVIDES fmod.dll i386 stateful
 *
 * SILENT STAND-IN for the FMOD core library, not the audio answer: the
 * fmod.dll half of fmodstudio_silent.c (whose comment says why and what
 * replaces it), pewasm's port/src/noita_fmod_silent.c "FMOD core" part. The
 * exports Noita imports from fmod.dll, all succeeding silently; the core
 * system they are called on is fmodstudio_silent.c's, so the real library
 * is never initialised and starts no thread. __stdcall members: the callee
 * pops `this` and the arguments. */

#define FMOD_OK 0u

/* The master channel group: a block only these functions see. */
static uint32_t master_group[3] = {0x42555331u /* BUS */, 0, 0};

#define OK(nargs) return gp_return_eax(ctx, ret, 4u * (nargs), FMOD_OK)
#define BODY(name) static GEX_PROVIDER(name)
#define NOOP(name, nargs) \
    BODY(name) {          \
        (void)depth;      \
        (void)need;       \
        OK(nargs);        \
    }

/* getVersion(unsigned *version): FMOD 2.01.05, as pewasm reports. */
BODY(get_version) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 1);
    if (p) {
        GW(p) = 0x00020105u;
    }
    OK(2);
}
NOOP(set_3d_settings, 4)
NOOP(set_dsp_buffer_size, 3)
NOOP(set_software_channels, 2)
NOOP(set_output, 2)
BODY(get_master_group) {
    (void)depth, (void)need;
    uint32_t p = gp_arg(ctx, 1);
    if (p) {
        GW(p) = (uint32_t)(uintptr_t)master_group;
    }
    OK(2);
}
NOOP(channel_set_volume, 2)

GEX_PROVIDE(get_version, "?getVersion@System@FMOD@@QAG?AW4FMOD_RESULT@@PAI@Z")
GEX_PROVIDE(set_3d_settings, "?set3DSettings@System@FMOD@@QAG?AW4FMOD_RESULT@@MMM@Z")
GEX_PROVIDE(set_dsp_buffer_size, "?setDSPBufferSize@System@FMOD@@QAG?AW4FMOD_RESULT@@IH@Z")
GEX_PROVIDE(set_software_channels, "?setSoftwareChannels@System@FMOD@@QAG?AW4FMOD_RESULT@@H@Z")
GEX_PROVIDE(set_output, "?setOutput@System@FMOD@@QAG?AW4FMOD_RESULT@@W4FMOD_OUTPUTTYPE@@@Z")
GEX_PROVIDE(get_master_group, "?getMasterChannelGroup@System@FMOD@@QAG?AW4FMOD_RESULT@@PAPAVChannelGroup@2@@Z")
GEX_PROVIDE(channel_set_volume, "?setVolume@ChannelControl@FMOD@@QAG?AW4FMOD_RESULT@@M@Z")
