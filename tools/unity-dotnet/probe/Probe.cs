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
    static int updates;

    [RuntimeInitializeOnLoadMethod(RuntimeInitializeLoadType.BeforeSceneLoad)]
    static void Init()
    {
        Console.Error.WriteLine("[gexprobe] init");
        AppDomain.CurrentDomain.FirstChanceException += (s, e) =>
            Console.Error.WriteLine("[gexprobe] first-chance " + e.Exception.GetType().FullName + ": " + e.Exception.Message + "\n" + e.Exception.StackTrace);
        AppDomain.CurrentDomain.UnhandledException += (s, e) => Console.Error.WriteLine("[gexprobe] unhandled " + e.ExceptionObject);
        try { throw new InvalidOperationException("gexprobe self-test"); } catch (Exception) { Console.Error.WriteLine("[gexprobe] caught the self-test exception"); }
        var loop = PlayerLoop.GetCurrentPlayerLoop();
        var subs = loop.subSystemList;
        var n = new PlayerLoopSystem[subs.Length + 1];
        Array.Copy(subs, n, subs.Length);
        n[subs.Length] = new PlayerLoopSystem { type = typeof(GexProbe), updateDelegate = Tick };
        loop.subSystemList = n;
        PlayerLoop.SetPlayerLoop(loop);
    }

    static void Tick()
    {
        updates++;
        float now = Time.realtimeSinceStartup;
        if (now < next) return;
        next = now + 5f;
        try
        {
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
            Console.Error.WriteLine($"[gexprobe] cameras {Camera.allCamerasCount}");
            foreach (var c in Camera.allCameras)
                Console.Error.WriteLine($"[gexprobe]   camera {c.name} enabled {c.enabled} active {c.isActiveAndEnabled} display {c.targetDisplay} rect {c.pixelRect} tex {(c.targetTexture ? c.targetTexture.name : "-")}");
        }
        catch (Exception e) { Console.Error.WriteLine("[gexprobe] tick failed: " + e); }
    }
}
