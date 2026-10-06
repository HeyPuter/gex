// Diagnostics for Hollow Knight under gex's .NET Mono (docs/unity-dotnet.md):
// loaded by Unity through RuntimeInitializeOnLoads.json; logs every
// first-chance exception and, every few seconds of real time, the clock and
// the intro's state, from a player-loop system (no MonoBehaviour needed).
using System;
using UnityEngine;
using UnityEngine.LowLevel;

public static class GexProbe
{
    static float next;
    static int updates, preRenders, postRenders;

    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)]
    static void Init()
    {
        Console.Error.WriteLine("[gexprobe] init");
        AppDomain.CurrentDomain.FirstChanceException += (s, e) =>
            Console.Error.WriteLine("[gexprobe] first-chance " + e.Exception.GetType().FullName + ": " + e.Exception.Message + "\n" + e.Exception.StackTrace);
        AppDomain.CurrentDomain.UnhandledException += (s, e) => Console.Error.WriteLine("[gexprobe] unhandled " + e.ExceptionObject);
        try { throw new InvalidOperationException("gexprobe self-test"); } catch (Exception) { Console.Error.WriteLine("[gexprobe] caught the self-test exception"); }
        Camera.onPreRender += c => preRenders++;
        Camera.onPostRender += c => postRenders++;
        var loop = PlayerLoop.GetCurrentPlayerLoop();
        var subs = loop.subSystemList;
        var n = new PlayerLoopSystem[subs.Length + 1];
        Array.Copy(subs, n, subs.Length);
        n[subs.Length] = new PlayerLoopSystem { type = typeof(GexProbe), updateDelegate = Tick };
        loop.subSystemList = n;
        PlayerLoop.SetPlayerLoop(loop);
    }

    static int inspected;

    // GEXPROBE_INSPECT=Type, Assembly: the first objects of that type (up to 5, once per 4 ticks,
    // 3 times): every UnityEngine.Object-typed field up its class chain, alive / destroyed / null,
    // and how many objects of the field's type are loaded.
    static void Inspect()
    {
        var tn = Environment.GetEnvironmentVariable("GEXPROBE_INSPECT");
        if (string.IsNullOrEmpty(tn) || inspected >= 3) return;
        if (updates % 4 != 0) return;
        inspected++;
        var t = Type.GetType(tn);
        if (t == null) { Console.Error.WriteLine($"[gexprobe] inspect: no type {tn}"); return; }
        var objs = Resources.FindObjectsOfTypeAll(t);
        Console.Error.WriteLine($"[gexprobe] inspect {t.FullName}: {objs.Length} objects");
        for (int i = 0; i < objs.Length && i < 5; i++)
        {
            var o = objs[i];
            Console.Error.WriteLine($"[gexprobe]   {(o ? o.name : "destroyed")}");
            for (var c = t; c != null && c != typeof(UnityEngine.Object); c = c.BaseType)
                foreach (var f in c.GetFields(System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.DeclaredOnly))
                    if (typeof(UnityEngine.Object).IsAssignableFrom(f.FieldType))
                    {
                        var v = f.GetValue(o) as UnityEngine.Object;
                        Console.Error.WriteLine($"[gexprobe]     {c.Name}.{f.Name} : {f.FieldType.Name} = {(ReferenceEquals(v, null) ? "null" : (v ? v.name : "DESTROYED"))}; loaded {Resources.FindObjectsOfTypeAll(f.FieldType).Length}");
                    }
        }
    }

    // GEXPROBE_FONTS=1: every tick, each loaded Font's material and atlas texture (alive, size,
    // GL name from GetNativeTexturePtr), and every Font.textureRebuilt event.
    static bool fontsHooked;
    static void Fonts()
    {
        if (Environment.GetEnvironmentVariable("GEXPROBE_FONTS") != "1") return;
        if (!fontsHooked)
        {
            fontsHooked = true;
            Font.textureRebuilt += f => Console.Error.WriteLine($"[gexprobe] textureRebuilt {(f ? f.name : "destroyed")} frame {Time.frameCount}");
        }
        foreach (Font f in Resources.FindObjectsOfTypeAll(typeof(Font)))
        {
            var m = f.material; var tx = ReferenceEquals(m, null) || !m ? null : m.mainTexture;
            string ts = ReferenceEquals(tx, null) ? "null" : !tx ? "DESTROYED" : $"{tx.name} {tx.width}x{tx.height} gl {tx.GetNativeTexturePtr()} id {tx.GetInstanceID()}";
            Console.Error.WriteLine($"[gexprobe] font {f.name} dynamic {f.dynamic} material {(ReferenceEquals(m, null) ? "null" : m ? m.name : "DESTROYED")} tex {ts}");
        }
    }

    static void Tick()
    {
        updates++;
        float now = Time.realtimeSinceStartup;
        if (now < next) return;
        next = now + (Environment.GetEnvironmentVariable("GEXPROBE_EVERY") is string ev && float.TryParse(ev, out var evs) ? evs : 5f);
        try
        {
            Console.Error.WriteLine($"[gexprobe] gc {GC.CollectionCount(0)}/{GC.CollectionCount(1)}/{GC.CollectionCount(2)} heap {GC.GetTotalMemory(false) >> 20} MB");
            Console.Error.WriteLine($"[gexprobe] real {now:F1} time {Time.time:F2} unscaled {Time.unscaledTime:F2} dt {Time.deltaTime:F4} scale {Time.timeScale} frame {Time.frameCount} ticks {updates}");
            var sm = GameObject.Find("StartManager");
            if (sm != null)
            {
                var a = sm.GetComponent<Animator>();
                if (a != null)
                {
                    var st = a.GetCurrentAnimatorStateInfo(0);
                    foreach (var prm in a.parameters)
                        Console.Error.WriteLine($"[gexprobe]   param {prm.name} {prm.type} bool {(prm.type == AnimatorControllerParameterType.Bool || prm.type == AnimatorControllerParameterType.Trigger ? a.GetBool(prm.name).ToString() : "-")}");
                    var nxt = a.GetNextAnimatorStateInfo(0);
                    Console.Error.WriteLine($"[gexprobe]   in transition {a.IsInTransition(0)} next {nxt.shortNameHash} hashes Start {Animator.StringToHash("Start")}");
                    foreach (var mb in sm.GetComponents<MonoBehaviour>())
                    {
                        var t = mb.GetType();
                        Console.Error.WriteLine($"[gexprobe]   component {t.FullName} enabled {mb.enabled}");
                        foreach (var f in t.GetFields(System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Public))
                            if (f.FieldType.IsPrimitive || f.FieldType == typeof(string) || f.FieldType.IsEnum)
                                Console.Error.WriteLine($"[gexprobe]     {f.Name} = {f.GetValue(mb)}");
                            else if (typeof(UnityEngine.Object).IsAssignableFrom(f.FieldType))
                            {
                                var v = f.GetValue(mb) as UnityEngine.Object;
                                var all = Resources.FindObjectsOfTypeAll(f.FieldType);
                                Console.Error.WriteLine($"[gexprobe]     {f.Name} : {f.FieldType.FullName} = {(ReferenceEquals(v, null) ? "null" : (v ? v.name : "destroyed"))}; loaded of that type: {all.Length}");
                            }
                        var m = t.GetMethod("CheckIsLanguageSet");
                        if (m != null) Console.Error.WriteLine($"[gexprobe]     CheckIsLanguageSet() = {m.Invoke(mb, null)}");
                    }
                    var pt = Type.GetType("Platform, Assembly-CSharp");
                    var cur = pt?.GetProperty("Current")?.GetValue(null);
                    if (cur != null)
                        foreach (var n in new[] { "IsSharedDataMounted", "ShowLanguageSelect", "IsPlayerPrefsLoaded" })
                            Console.Error.WriteLine($"[gexprobe]   Platform.Current ({cur.GetType().Name}).{n} = {cur.GetType().GetProperty(n)?.GetValue(cur)}");
                    Console.Error.WriteLine($"[gexprobe] StartManager active {sm.activeInHierarchy} animator enabled {a.enabled} active {a.isActiveAndEnabled} init {a.isInitialized} speed {a.speed} state {st.shortNameHash} t {st.normalizedTime:F2} loading {Animator.StringToHash("LoadingIcon")} ctrl {(a.runtimeAnimatorController ? a.runtimeAnimatorController.name : "none")}");
                }
            }
            else Console.Error.WriteLine("[gexprobe] no StartManager");
            Console.Error.WriteLine($"[gexprobe] cameras {Camera.allCamerasCount} prerender {preRenders} postrender {postRenders} focused {Application.isFocused} runInBackground {Application.runInBackground} screen {Screen.width}x{Screen.height} device {SystemInfo.graphicsDeviceType} scene {UnityEngine.SceneManagement.SceneManager.GetActiveScene().name}");
            Inspect(); Fonts();
            Console.Error.WriteLine($"[gexprobe] displays {Display.displays.Length} main active {Display.main.active} rendering {Display.main.renderingWidth}x{Display.main.renderingHeight} system {Display.main.systemWidth}x{Display.main.systemHeight} fullscreen {Screen.fullScreenMode} playing {Application.isPlaying} vsync {QualitySettings.vSyncCount} targetfps {Application.targetFrameRate}");
            if (Environment.GetEnvironmentVariable("GEXPROBE_RENDER") == "1")
                foreach (var c in Camera.allCameras)
                {
                    try { c.Render(); Console.Error.WriteLine($"[gexprobe]   rendered {c.name} by hand; prerender now {preRenders}"); }
                    catch (Exception e) { Console.Error.WriteLine($"[gexprobe]   {c.name}.Render() threw {e.GetType().Name}: {e.Message}"); }
                }
            foreach (var c in Camera.allCameras)
                Console.Error.WriteLine($"[gexprobe]   camera {c.name} enabled {c.enabled} active {c.isActiveAndEnabled} display {c.targetDisplay} rect {c.pixelRect} tex {(c.targetTexture ? c.targetTexture.name : "-")}");
        }
        catch (Exception e) { Console.Error.WriteLine("[gexprobe] tick failed: " + e); }
    }
}
