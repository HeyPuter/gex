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
  // Guest code calling back into managed code through function pointers.
  public delegate double Callback(int a, double b, Vec3 v);
  static double OnCallback(int a, double b, Vec3 v) => a * 100 + b + v.x + v.y + v.z;
  static Callback keep = OnCallback;
  [UnmanagedCallersOnly] static int Unmanaged(int x, long y) => (int)(x * 3 + y);
  [DllImport("__Internal")] static extern double gex_callback(IntPtr fn, int a, double b);
  [DllImport("__Internal")] static extern unsafe int gex_unmanaged(delegate* unmanaged<int, long, int> fn, int x);
  public static unsafe string Back() {
    var p = Marshal.GetFunctionPointerForDelegate(keep);
    var r = gex_callback(p, 7, 0.5);
    var u = gex_unmanaged(&Unmanaged, 5);
    return "callback " + r.ToString("F2") + "; unmanaged " + u;
  }
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
  public static string Make(int i) => "object " + i + " " + new string('x', i % 50);
  public static int Churn(int rounds) {
    int n = 0;
    for (int r = 0; r < rounds; r++) {
      var l = new List<object>();
      for (int i = 0; i < 20000; i++) l.Add(new byte[64 + i % 300]);
      n += l.Count;
      GC.Collect();
    }
    return n;
  }
  // A managed-heavy workload for the wasm JIT (virtual and interface calls, generics, boxing,
  // enums, reflection): the same checksum interpreted and JITted.
  abstract class Shape { public abstract double Area (); public virtual int Sides => 0; }
  sealed class Sq : Shape { public double S; public override double Area () => S * S; public override int Sides => 4; }
  sealed class Ci : Shape { public double R; public override double Area () => 3.0 * R * R; }
  class Tri : Shape { public double B, H; public override double Area () => B * H / 2; public override int Sides => 3; }
  interface IStep { int Step (int x); }
  struct Add3 : IStep { public int Step (int x) => x + 3; }
  class Mul2 : IStep { public int Step (int x) => x * 2; }
  struct Pt { public int X; public long Y; }
  static int Run<TS> (TS s, int x) where TS : IStep => s.Step (x);
  [MethodImpl(MethodImplOptions.NoInlining)]
  static int Thrower (int r) { if (r % 30 == 0) throw new InvalidOperationException ("odd " + r); return 3; }
  public static long Work (int n) {
    long sum = 0;
    var shapes = new List<Shape> ();
    for (int i = 0; i < 64; i++)
      shapes.Add (i % 3 == 0 ? new Sq { S = i } : i % 3 == 1 ? (Shape) new Ci { R = i } : new Tri { B = i, H = 2 });
    IStep[] steps = { new Add3 (), new Mul2 () };
    var d = new Dictionary<int, string> ();
    var fx = typeof (Pt).GetField ("X");
    for (int r = 0; r < n; r++) {
      foreach (var sh in shapes) sum += (long) sh.Area () + sh.Sides;
      for (int i = 0; i < 100; i++) sum += steps [i & 1].Step (i) + Run (new Add3 (), i);
      var e = (DayOfWeek) Enum.ToObject (typeof (DayOfWeek), r % 7);
      sum += e.ToString ().Length;
      object boxed = new Pt { X = r, Y = 2L * r };
      sum += (int) fx.GetValue (boxed);
      d [r % 50] = r.ToString ();
      sum += d [r % 50].Length;
      if (r % 10 == 0) {
        try { sum += Thrower (r); } catch (InvalidOperationException x) { sum += x.Message.Length; } finally { sum += 1; }
        try { sum += int.Parse (r % 20 == 0 ? "x" : "12"); } catch (FormatException) { sum += 5; }
      }
    }
    return sum;
  }
  public static string Hello(string s) => "hello " + s + " from " + typeof(T).Assembly.GetName().Name + " ptr=" + IntPtr.Size;
}
