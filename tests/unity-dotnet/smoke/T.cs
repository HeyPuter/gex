using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
public struct Vec3 { public float x, y, z; }
[StructLayout(LayoutKind.Sequential)]
public struct Big { public long a, b, c; public double d; }
public static class T {
  [MethodImpl(MethodImplOptions.InternalCall)] static extern int Native(int x);
  public static int Run(int n) {
    var l = new List<string>();
    for (int i = 0; i < n; i++) l.Add(i.ToString());
    GC.Collect();
    var d = new Dictionary<string, int>();
    foreach (var s in l) d[s] = s.Length;
    return Native(l.Count) + d[l[n - 1]];
  }
  public static int Compute(int n) {
    var l = new List<string>();
    for (int i = 0; i < n; i++) l.Add(i.ToString());
    GC.Collect();
    var d = new Dictionary<string, int>();
    foreach (var s in l) d[s] = s.Length;
    return l.Count * 1000 + d[l[n - 1]];
  }
  // Calls into the embedding program (embed.c): internal calls it registers
  // and P/Invokes it exports.
  [MethodImpl(MethodImplOptions.InternalCall)] static extern int Twice(int x);
  [MethodImpl(MethodImplOptions.InternalCall)] static extern string Concat(string a, string b);
  [MethodImpl(MethodImplOptions.InternalCall)] static extern void Throws(string msg);
  [DllImport("__Internal")] static extern double gex_mix(int a, double b, Vec3 v, long c, float f);
  [DllImport("__Internal")] static extern Vec3 gex_scale(Vec3 v, float k);
  [DllImport("__Internal")] static extern Big gex_big(long a, Big b, long c, long d, long e, long f, long g, long h);
  public static string Guest() {
    var s = "twice " + Twice(21) + "; concat " + Concat("guest ", "strings");
    var v = new Vec3 { x = 1, y = 2, z = 3 };
    s += "; mix " + gex_mix(2, 0.5, v, 1L << 40, 0.25f).ToString("F2");
    var w = gex_scale(v, 2);
    s += "; scale " + w.x + "," + w.y + "," + w.z;
    var b = gex_big(1, new Big { a = 10, b = 20, c = 30, d = 1.5 }, 2, 3, 4, 5, 6, 7);
    s += "; big " + b.a + "," + b.b + "," + b.c + "," + b.d;
    try { Throws("from the guest"); s += "; no throw"; }
    catch (ArgumentException e) { s += "; caught " + e.Message; }
    return s;
  }
  public static string Hello(string s) => "hello " + s + " from " + typeof(T).Assembly.GetName().Name + " ptr=" + IntPtr.Size;
}
