using System;
using System.Collections.Generic;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;

[StructLayout(LayoutKind.Sequential)]
public struct Vec3 { public float x, y, z; }
[StructLayout(LayoutKind.Sequential)]
public struct Big { public long a, b, c; public double d; }
// The liveness walk's graph (mono_unity_liveness_*, embed.c): every Leaf reachable from an
// LvRoot through fields, arrays, value-type fields and arrays of structs, a list, cycles.
public class Leaf { public int id; public object next; }
public class SubLeaf : Leaf { }
public struct Pair { public object a; public int x; public Leaf b; }
public class LvRoot { public object[] arr; public Pair p; public Pair[] pairs; public List<Leaf> list; public LvRoot self; public string s; }
public static class T {
  // 3n+1 distinct Leafs: n in arr (half SubLeaf, each .next the following one, a cycle), one in
  // p.b, n in pairs[i].b (pairs[i].a repeats arr[i]), n in list.
  // Array.Clear and Span.Clear over reference, value and mixed element types, many offsets
  // (Hollow Knight's first wasm-JIT fault was in SpanHelpers.ClearWithReferences under Array.Clear).
  public struct Mixed { public object o; public long v; }
  public static string Clears(int n) {
    long sum = 0;
    for (int len = 1; len <= n; len += 7) {
      var a = new object[len]; for (int i = 0; i < len; i++) a[i] = i;
      int from = len / 3, cnt = len - from - len / 4;
      Array.Clear(a, from, cnt);
      for (int i = 0; i < len; i++) if (a[i] == null) sum += i + 1;
      var b = new int[len]; for (int i = 0; i < len; i++) b[i] = i + 1;
      Array.Clear(b, from, cnt);
      for (int i = 0; i < len; i++) sum += b[i];
      var c = new Mixed[len]; for (int i = 0; i < len; i++) c[i] = new Mixed { o = "s", v = i };
      Array.Clear(c, from, cnt);
      for (int i = 0; i < len; i++) if (c[i].o == null) sum += 3 * c[i].v + 1;
      var l = new List<string>(); for (int i = 0; i < len; i++) l.Add("x"); l.Clear(); sum += l.Count;
      new Span<object>(a).Slice(len / 2).Clear();
      for (int i = 0; i < len; i++) if (a[i] == null) sum += 1;
    }
    return "clears " + sum;
  }
  // Delegates made in code the wasm JIT compiled (ldftn, ldvirtftn, a boxed struct's override, a
  // closure): their constructor runs on the interpreter, which takes the JIT's function pointers as
  // its own (Hollow Knight's tk2dSpriteAnimator.Start read a NULL method from a native entry).
  public class DShape { public int k; public virtual int Area(int x) { return x * k; } public int Plain(int x) { return x + k; } }
  public class DSquare : DShape { public override int Area(int x) { return x * x + k; } }
  public struct Tag { public int v; public override string ToString() { return "tag" + v; } }
  static int DSq(int x) { return x * x; }
  // GC.GetTotalMemory(false) is mono_gc_get_used_size, which patch 0007 caches between collections:
  // it must still see objects promoted to the major heap, give the same answer when asked again,
  // and drop once they are collected.
  // In chunks, each dropped on its own: a stale slot on the (conservatively scanned) stack can keep the
  // last chunk alive, not all of them.
  static byte[][][] usKeep = new byte[40][][];
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
  static void UsFill(int n) {
    for (int c = 0; c < usKeep.Length; c++) {
      usKeep[c] = new byte[n / usKeep.Length][];
      for (int i = 0; i < usKeep[c].Length; i++) usKeep[c][i] = new byte[1000];
    }
  }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
  static void UsDrop() { for (int c = 0; c < usKeep.Length; c++) usKeep[c] = null; }
  public static string UsedSize(int n) {
    GC.Collect();
    long before = GC.GetTotalMemory(false);
    UsFill(n);
    GC.Collect();
    long held = GC.GetTotalMemory(false), again = GC.GetTotalMemory(false);
    UsDrop();
    GC.Collect();
    long after = GC.GetTotalMemory(false);
    return "usedsize " + (held - before >= (long)n * 1000 ? "grew" : "flat " + (held - before)) +
      (again == held ? " stable" : " unstable") +
      (held - after >= (long)n * 800 ? " shrank" : " kept " + (held - after) + " of " + (held - before));
  }
  // Checked arithmetic and unsigned-to-double conversions, each in its own clause-free method so the wasm JIT
  // compiles it (OP_IADDCC/ISUBCC/LADDCC/LSUBCC + COND_EXC, OP_IMUL_OVF[_UN], OP_[I|L]CONV_TO_R_UN).
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int CAdd(int a, int b) => checked(a + b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int CSub(int a, int b) => checked(a - b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static uint CAddU(uint a, uint b) => checked(a + b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static uint CSubU(uint a, uint b) => checked(a - b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int CMul(int a, int b) => checked(a * b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static uint CMulU(uint a, uint b) => checked(a * b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static long CAddL(long a, long b) => checked(a + b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static ulong CSubUL(ulong a, ulong b) => checked(a - b);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static double UToD(uint a) => a;
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static double ULToD(ulong a) => a;
  static char Ovf(Func<long> f, long want) {
    try { return f() == want ? '.' : 'W'; } catch (OverflowException) { return 'O'; }
  }
  public static string Checked(int n) {
    var sb = new System.Text.StringBuilder("checked ");
    for (int r = 0; r < n; r++) {
      if (r > 0 && r < n - 1) { CAdd(r, r); CSub(r, r); CAddU(1, 2); CSubU(3, 2); CMul(r, 3); CMulU(2, 3); CAddL(r, 1); CSubUL(5, 1); UToD(7); ULToD(7); continue; }
      sb.Append(Ovf(() => CAdd(int.MaxValue - 1, 1), int.MaxValue)).Append(Ovf(() => CAdd(int.MaxValue, 1), 0));
      sb.Append(Ovf(() => CAdd(int.MinValue, -1), 0)).Append(Ovf(() => CSub(int.MinValue, 1), 0)).Append(Ovf(() => CSub(-5, 7), -12));
      sb.Append(Ovf(() => CAddU(uint.MaxValue, 1), 0)).Append(Ovf(() => CAddU(uint.MaxValue - 1, 1), uint.MaxValue));
      sb.Append(Ovf(() => CSubU(1, 2), 0)).Append(Ovf(() => CSubU(2, 2), 0));
      sb.Append(Ovf(() => CMul(65536, 32768), 0)).Append(Ovf(() => CMul(-65536, 32768), int.MinValue)).Append(Ovf(() => CMul(46341, -46340), -2147441940));
      sb.Append(Ovf(() => CMulU(65536, 65536), 0)).Append(Ovf(() => CMulU(65535, 65537), uint.MaxValue));
      sb.Append(Ovf(() => CAddL(long.MaxValue, 1), 0)).Append(Ovf(() => CAddL(long.MinValue, long.MaxValue), -1));
      sb.Append(Ovf(() => unchecked((long)CSubUL(0, 1)), 0)).Append(Ovf(() => unchecked((long)CSubUL(ulong.MaxValue, 1)), unchecked((long)(ulong.MaxValue - 1))));
      sb.Append(UToD(uint.MaxValue) == 4294967295.0 ? '.' : 'W').Append(ULToD(ulong.MaxValue) == 18446744073709551615.0 ? '.' : 'W');
      sb.Append(' ');
    }
    return sb.ToString().TrimEnd();
  }
  // ldarga of scalar arguments (OP_LDADDR of an argument): the wasm JIT homes the argument in its addr frame.
  public struct PtrBox { public IntPtr v; }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static PtrBox ArgAsBox(IntPtr p) => Unsafe.As<IntPtr, PtrBox>(ref p);
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int ArgInt(int x) { ref int r = ref x; r += 5; return x * 3 + Bump(ref x); }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int ArgByte(byte b, sbyte c) { ref byte r = ref b; r = (byte)(r + 250); ref sbyte q = ref c; q = (sbyte)(q - 100); return b * 1000 + c; }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static double ArgDouble(double d, float f) { ref double r = ref d; r *= 2; ref float g = ref f; g += 0.5f; return d + f; }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static long ArgLong(long l, int k) { ref long r = ref l; for (int i = 0; i < k; i++) r = r * 3 + i; return l; }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int Bump(ref int v) { v++; return v; }
  // ldarga of a REFERENCE argument: a constrained call on a generic T, and a write through the address.
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int HashOf<TK>(TK x) => x.GetHashCode();
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static void SetRef(ref string t, string v) { t = v; }
  [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)] static int RefArg(string s, int i) { SetRef(ref s, s + i); GC.Collect(0); return s.Length + HashOf(s).GetHashCode() * 0 + HashOf(i); }
  public static string ArgAddr(int n) {
    long acc = 0;
    for (int i = 0; i < n; i++) {
      acc += ArgAsBox((IntPtr)(i * 8)).v.ToInt64();
      acc += ArgInt(i);
      acc += ArgByte((byte)i, (sbyte)i);
      acc += (long)ArgDouble(i, i);
      acc += ArgLong(i, 3);
      if (i % 8 == 0) acc += RefArg("ref", i);
    }
    return "argaddr " + acc;
  }
  // A leave that exits two or three try-finally blocks at once (return / break from the innermost try):
  // mono emits one OP_CALL_HANDLER per finally, which the wasm JIT once refused as a "complex continuation".
  [MethodImpl(MethodImplOptions.NoInlining)] static int Fin2(int i, ref int log) {
    try { try { if (i % 3 == 0) return i * 2; log += 1; } finally { log += 10; } log += 100; }
    finally { log += 1000; }
    return -1;
  }
  [MethodImpl(MethodImplOptions.NoInlining)] static int Fin3(int n, ref int log) {
    int k = 0;
    for (; k < n; k++) {
      try { try { try { if (k == 5) break; if (k % 2 == 0) continue; log += k; } finally { log += 2; } }
        finally { log *= 3; log %= 1000003; } }
      finally { log ^= k; }
    }
    return k;
  }
  [MethodImpl(MethodImplOptions.NoInlining)] static int FinThrow(int i, ref int log) {
    try { try { try { if (i % 4 == 1) throw new InvalidOperationException(); if (i % 4 == 2) return 7; } finally { log += 3; } }
      finally { log += 5; } }
    catch (InvalidOperationException) { log += 11; }
    return 1;
  }
  // ldarga in a method with EH clauses: an int argument and a string argument whose addresses are taken
  // inside the try, mutated through the address, and read again in the catch / finally and after them.
  [MethodImpl(MethodImplOptions.NoInlining)] static void BumpRef(ref int v, int by) { v += by; if (v % 5 == 0) throw new InvalidOperationException(); }
  [MethodImpl(MethodImplOptions.NoInlining)] static void AppendRef(ref string t) { t = t + "!"; }
  [MethodImpl(MethodImplOptions.NoInlining)] static string FinArg(int i, string s) {
    int caught = 0;
    try {
      BumpRef(ref i, 3);
      AppendRef(ref s);
      s = s + i.ToString();
    } catch (InvalidOperationException) {
      caught = i;
      AppendRef(ref s);
    } finally {
      finCount += i & 7;
    }
    return s + "/" + i + "/" + caught;
  }
  static int finCount;
  // A value chosen inside the try and used after the leave: out-of-SSA puts its phi move between
  // the CALL_HANDLER and the leave's branch.
  [MethodImpl(MethodImplOptions.NoInlining)] static string Str(int i) => i.ToString();
  [MethodImpl(MethodImplOptions.NoInlining)] static string FinMove(int i) {
    string s = null;
    try {
      if (i % 3 == 0) { s = "a" + Str(i); return s; }
      if (i % 3 == 1) { s = "b"; } else { s = "c" + Str(i * 2); }
    } finally { finCount++; }
    return s + Str(i);
  }
  static readonly object srLock = new object();
  static List<string> srLoading;
  static int srRec;
  static Dictionary<string, string> srTable = new Dictionary<string, string> { { "k1", "v1" }, { "k2", "v2" } };
  // SR.InternalGetResourceString's shape: lock + catch-rethrow + finally, leaves carrying a value.
  [MethodImpl(MethodImplOptions.NoInlining)] static string SrLike(string key) {
    if (key.Length == 0) return key;
    bool lockTaken = false;
    try {
      System.Threading.Monitor.Enter(srLock, ref lockTaken);
      if (srLoading != null && srLoading.Count > 0 && srLoading.LastIndexOf(key) >= 0) {
        if (srRec > 0) return key;
        srRec++;
      }
      srLoading ??= new List<string>();
      srLoading.Add(key);
      srTable.TryGetValue(key, out string s);
      if (key == "boom") throw new InvalidOperationException();
      srLoading.RemoveAt(srLoading.Count - 1);
      return s ?? key;
    } catch {
      if (lockTaken) { srLoading = null; }
      throw;
    } finally {
      if (lockTaken) System.Threading.Monitor.Exit(srLock);
    }
  }
  static string srOut;
  [MethodImpl(MethodImplOptions.NoInlining)] static void SrLike2(string key) {
    if (key.Length == 0) { srOut = key; return; }
    bool lockTaken = false;
    try {
      System.Threading.Monitor.Enter(srLock, ref lockTaken);
      if (srLoading != null && srLoading.Count > 0 && srLoading.LastIndexOf(key) >= 0) {
        if (srRec > 0) { srOut = key; return; }
        srRec++;
      }
      srLoading ??= new List<string>();
      srLoading.Add(key);
      srTable.TryGetValue(key, out string s);
      if (key == "boom") throw new InvalidOperationException();
      srLoading.RemoveAt(srLoading.Count - 1);
      srOut = s ?? key;
    } catch {
      if (lockTaken) { srLoading = null; }
      throw;
    } finally {
      if (lockTaken) System.Threading.Monitor.Exit(srLock);
    }
  }
  public unsafe class UmsLike {
    long _position, _length = 1 << 20; long _offset = 3; byte[] arr = new byte[64]; public int acq;
    [MethodImpl(MethodImplOptions.NoInlining)] void Acquire(ref byte* p) { acq++; p = (byte*)0 + 1; }
    [MethodImpl(MethodImplOptions.NoInlining)] void Release() { acq--; }
    [MethodImpl(MethodImplOptions.NoInlining)] public int ReadByteU() {
      long pos = System.Threading.Interlocked.Read(ref _position);
      long len = System.Threading.Interlocked.Read(ref _length);
      if (pos >= len) return -1;
      System.Threading.Interlocked.Exchange(ref _position, pos + 1);
      int result;
      byte* pointer = null;
      try {
        Acquire(ref pointer);
        result = arr[(int)((long)(pointer - 1) + pos + _offset) & 63] + (int)pos;
      } finally {
        if (pointer != null) Release();
      }
      return result;
    }
  }
  static byte[] umsData = new byte[256];
  [MethodImpl(MethodImplOptions.NoInlining)] static unsafe int UmsRead(int i) {
    for (int k = 0; k < 256; k++) umsData[k] = (byte)(k * 7 + 3);
    fixed (byte* p = umsData) {
      using var u = new System.IO.UnmanagedMemoryStream(p, 256);
      u.Position = i % 200;
      int a = u.ReadByte(), b = u.ReadByte();
      return a * 3 + b;
    }
  }
  // stackalloc (OP_LOCALLOC / OP_LOCALLOC_IMM), constant and variable size, zero-initialised, called in a loop
  // so a leaked SP would overflow the stack; plus BinaryReader.ReadInt32's own stackalloc.
  [MethodImpl(MethodImplOptions.NoInlining)] static unsafe int Stack(int n) {
    byte* a = stackalloc byte[24];
    Span<int> b = stackalloc int[n % 37 + 1];
    int s = 0;
    for (int k = 0; k < 24; k++) s += a[k];
    for (int k = 0; k < b.Length; k++) { s += b[k]; b[k] = k * n; }
    for (int k = 0; k < b.Length; k++) s += b[k];
    a[3] = (byte)n; s += a[3];
    return s;
  }
  [MethodImpl(MethodImplOptions.NoInlining)] static long CMulL(long a, long b) => checked(a * b);
  [MethodImpl(MethodImplOptions.NoInlining)] static ulong CMulUL(ulong a, ulong b) => checked(a * b);
  static string MulOvf() {
    var sb = new System.Text.StringBuilder();
    long[] ls = { 0, 1, -1, 2, -2, 3, long.MaxValue, long.MinValue, 1L << 31, -(1L << 32), 3037000499, 3037000500, -3037000500 };
    foreach (long a in ls) foreach (long b in ls) {
      try { long r = CMulL(a, b); sb.Append((char)('a' + (int)((ulong)r % 26))); } catch (OverflowException) { sb.Append('O'); }
    }
    sb.Append(' ');
    ulong[] us = { 0, 1, 2, 3, ulong.MaxValue, ulong.MaxValue / 2, (ulong)uint.MaxValue, (ulong)uint.MaxValue + 1, 4294967296UL * 2 };
    foreach (ulong a in us) foreach (ulong b in us) {
      try { ulong r = CMulUL(a, b); sb.Append((char)('a' + (int)(r % 26))); } catch (OverflowException) { sb.Append('O'); }
    }
    return sb.ToString();
  }
  // Hollow Knight's hot interpreter legs (hg10), as shapes: Unity InputSystem's InputStateBlock.ReadFloat
  // (ldind.* by format, uint -> float through conv.r.un), List<ref>.AddRange (checked add in shared generic code,
  // OP_IADDCC) and a KeyCombo-style struct accessor that takes the address of a fixed buffer / local.
  public class Ev { public int id; public Ev(int i) { id = i; } }
  [MethodImpl(MethodImplOptions.NoInlining)] static float UNorm(uint v, uint min, uint max) => (float)(v - min) / (max - min);
  [MethodImpl(MethodImplOptions.NoInlining)] static float INorm(int v, int min, int max) => (float)(v - min) / (max - min);
  [MethodImpl(MethodImplOptions.NoInlining)] static unsafe float ReadFloatLike(void* p, int fmt) {
    switch (fmt) {
      case 0: return INorm(*(int*)p, int.MinValue, int.MaxValue);
      case 1: return UNorm(*(uint*)p, uint.MinValue, uint.MaxValue);
      case 2: return INorm(*(short*)p, short.MinValue, short.MaxValue);
      case 3: return UNorm(*(ushort*)p, ushort.MinValue, ushort.MaxValue);
      case 4: return UNorm(*(byte*)p, byte.MinValue, byte.MaxValue);
      case 5: return *(float*)p;
      default: return (float)*(double*)p;
    }
  }
  public unsafe struct Combo { fixed byte inc[8]; int n;
    public void Add(int k) { if (n < 8) { inc[n] = (byte)k; n++; } }
    [MethodImpl(MethodImplOptions.NoInlining)] public int GetInclude(int i) { fixed (byte* b = inc) return b[i]; } }
  public static unsafe string HotShapes(int n) {
    double facc = 0; long lacc = 0;
    var buf = stackalloc byte[8];
    var list = new List<Ev>();
    var add = new List<Ev> { new Ev(1), new Ev(2), new Ev(3) };
    var combo = new Combo();
    for (int k = 0; k < 8; k++) combo.Add(k * 7 + 1);
    for (int i = 0; i < n; i++) {
      *(ulong*)buf = (ulong)(uint)(i * 2654435761u) * 0x9E3779B97F4A7C15UL;
      if ((i & 7) == 5) *(float*)buf = i * 0.25f;
      if ((i & 7) >= 6) *(double*)buf = i * 0.125;
      facc += ReadFloatLike(buf, i & 7);
      list.AddRange(add);
      if (list.Count > 300) list.Clear();
      lacc += list.Count + combo.GetInclude(i & 7);
    }
    return "hotshapes " + lacc + " " + facc.ToString("F2", System.Globalization.CultureInfo.InvariantCulture);
  }
  public static string Localloc(int n) {
    long acc = 0;
    for (int i = 0; i < n; i++) acc += Stack(i);
    var ms = new System.IO.MemoryStream();
    var w = new System.IO.BinaryWriter(ms);
    for (int i = 0; i < 64; i++) w.Write(i * 1000003);
    ms.Position = 0;
    var r = new System.IO.BinaryReader(ms);
    for (int i = 0; i < 64; i++) acc += r.ReadInt32() % 9973;
    return "localloc " + acc + " " + MulOvf();
  }
  public static string NestedFinally(int n) {
    long acc = 0; int log = 0; var ums = new UmsLike();
    for (int i = 0; i < n; i++) {
      acc += Fin2(i, ref log);
      acc += Fin3(i, ref log);
      acc += FinThrow(i, ref log);
      acc += FinMove(i).Length * 7 + FinMove(i)[0];
      { string fa = FinArg(i, "s"); acc += fa.Length * 13 + fa[fa.Length - 1]; }
      string sk = (i % 4 == 0) ? "k1" : (i % 4 == 1) ? "k2" : (i % 4 == 2) ? "zz" : "boom";
      try { acc += SrLike(sk)[0]; acc += SrLike(sk).Length; } catch (InvalidOperationException) { acc += 5; }
      try { SrLike2(sk); acc += srOut.Length; } catch (InvalidOperationException) { acc += 3; }
      acc += ums.ReadByteU() + ums.acq;
      acc += UmsRead(i);
      acc = acc * 31 + log;
      acc %= 1000000007;
    }
    return "finally " + acc + " " + log + " " + finCount;
  }
  // Calls into System.Reflection from a hot method (PlayMaker's LoadActionField shape): the wasm JIT once
  // refused every such caller ("residual reflection"), a guard needed only once Reflection.Emit is in use.
  public class RBox { public int a; public string s; }
  [MethodImpl(MethodImplOptions.NoInlining)] static int ReflGet(System.Reflection.FieldInfo fa, System.Reflection.FieldInfo fs, RBox b, int i) {
    fa.SetValue(b, (int)fa.GetValue(b) + i);
    var f2 = System.Reflection.FieldInfo.GetFieldFromHandle(fa.FieldHandle);
    string s = (string)fs.GetValue(b);
    return (int)f2.GetValue(b) + (s == null ? 0 : s.Length) + (System.Reflection.MethodBase.GetCurrentMethod().Name.Length)
      + System.Reflection.Assembly.GetExecutingAssembly().GetName().Name.Length * 1000;
  }
  // Stack-walking callers: GetCurrentMethod / GetExecutingAssembly (JIT intrinsics) and Type.GetType(string),
  // which searches the CALLING assembly and so must stay in the interpreter.
  [MethodImpl(MethodImplOptions.NoInlining)] static int StackWalkers(int i) {
    int r = System.Reflection.MethodBase.GetCurrentMethod().Name.Length * 100;
    r += System.Reflection.Assembly.GetExecutingAssembly().GetName().Name.Length * 10;
    r += Type.GetType("T") != null ? 1 : 0;
    return r + i;
  }
  public static string Reflect(int n) {
    var b = new RBox { s = "xyz" };
    var fa = typeof(RBox).GetField("a"); var fs = typeof(RBox).GetField("s");
    long acc = 0;
    for (int i = 0; i < n; i++) { acc += ReflGet(fa, fs, b, i); acc = acc * 7 + StackWalkers(i); acc %= 1000000007; }
    return "reflect " + acc + " " + b.a;
  }
  public static string Delegates(int n) {
    long sum = 0; string tags = "";
    for (int i = 0; i < n; i++) {
      Func<int, int> st = DSq;
      DShape s = i % 2 == 0 ? new DShape { k = i } : new DSquare { k = i };
      Func<int, int> pl = s.Plain, vi = s.Area;
      object o = new Tag { v = i };
      Func<string> ts = o.ToString;
      int c = i; Func<int, int> cl = x => x + c;
      sum += st(i) + pl(3) + vi(4) + cl(5);
      if (i < 3) tags += ts();
    }
    return "delegates " + sum + " " + tags;
  }
  // Call-transition costs (EMBED_BENCH): ns per call for a tiny callee, from a caller in the same
  // tier, from JIT code into a method the wasm JIT refuses (it holds a calli, never taken), and from
  // such a method into a JIT-compiled one. With GEXMONO_WASM_JIT=0 every leg is the interpreter's.
  [MethodImpl(MethodImplOptions.NoInlining)] static int BInc(int x) { return x + 1; }
  static unsafe delegate*<int, int> bnever = null;
  [MethodImpl(MethodImplOptions.NoInlining)] static unsafe int BIncInterp(int x) { if (x == int.MinValue) return bnever(x); return x + 1; }
  static int BLoopSame(int n) { int a = 0; for (int i = 0; i < n; i++) a = BInc(a); return a; }
  static int BLoopToInterp(int n) { int a = 0; for (int i = 0; i < n; i++) a = BIncInterp(a); return a; }
  static unsafe int BLoopFromInterp(int n) {
    int a = 0;
    if (n == int.MinValue) return bnever(n);
    for (int i = 0; i < n; i++) a = BInc(a);
    return a;
  }
  static unsafe int BLoopInterpInterp(int n) {
    int a = 0;
    if (n == int.MinValue) return bnever(n);
    for (int i = 0; i < n; i++) a = BIncInterp(a);
    return a;
  }
  [MethodImpl(MethodImplOptions.InternalCall)] static extern int Inc(int x);
  // A guest InternalCall: from JIT code it is a direct call (mono_wasm_jit_gex_native_call); "icall-interp" is the
  // same call from a method the JIT refuses, i.e. the interpreter's own path.
  static int BLoopIcall(int n) { int a = 0; for (int i = 0; i < n; i++) a = Inc(a); return a; }
  static unsafe int BLoopIcallInterp(int n) {
    int a = 0;
    if (n == int.MinValue) return bnever(n);
    for (int i = 0; i < n; i++) a = Inc(a);
    return a;
  }
  public static string CallBench(int n) {
    var sb = new System.Text.StringBuilder("callbench ns/call:");
    foreach (var (name, f) in new (string, Func<int, int>)[] { ("same", BLoopSame), ("to-refused", BLoopToInterp), ("from-refused", BLoopFromInterp), ("refused-refused", BLoopInterpInterp), ("icall", BLoopIcall), ("icall-interp", BLoopIcallInterp) }) {
      f(1000); f(1000);   // past the JIT threshold, compiled
      var sw = System.Diagnostics.Stopwatch.StartNew();
      int r = f(n);
      sw.Stop();
      sb.Append($" {name} {sw.Elapsed.TotalMilliseconds * 1e6 / n:F1}");
      if (r != n) sb.Append("(wrong)");
    }
    return sb.ToString();
  }
  public static object LvGraph(int n) {
    var r = new LvRoot { arr = new object[n], pairs = new Pair[n], list = new List<Leaf>(), s = "not a leaf" };
    for (int i = 0; i < n; i++) r.arr[i] = i % 2 == 0 ? new SubLeaf { id = i } : new Leaf { id = i };
    for (int i = 0; i < n; i++) ((Leaf)r.arr[i]).next = r.arr[(i + 1) % n];
    r.p = new Pair { a = "x", x = 1, b = new Leaf { id = -1 } };
    for (int i = 0; i < n; i++) { r.pairs[i] = new Pair { a = r.arr[i], x = i, b = new Leaf { id = 1000 + i } }; r.list.Add(new Leaf { id = 2000 + i }); }
    r.self = r;
    return r;
  }
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
  [MethodImpl(MethodImplOptions.InternalCall)] static extern long Accum(ref long acc, float f, double d, sbyte b, ushort c, long x);
  [MethodImpl(MethodImplOptions.InternalCall)] static extern bool IsOdd(int x);
  [DllImport("__Internal")] static extern long gex_mad(long a, int b, double c, float d, long e, int f, int g);
  // Clause-free, so the wasm JIT compiles them and calls the guest directly (mono_wasm_jit_gex_native_call).
  [MethodImpl(MethodImplOptions.NoInlining)] static long NatLoop(int n) {
    long acc = 0, r = 0;
    for (int i = 0; i < n; i++) {
      r += Accum(ref acc, i * 0.5f, i * 0.25, (sbyte)(-i), (ushort)(60000 + i), (long)i << 33);
      if (IsOdd(i)) r += Twice(i);
      r += Concat("a", i.ToString()).Length;
      r += gex_mad(i, 3, i * 0.75, 2.5f, -i, 7, i);
    }
    return r ^ acc;
  }
  [MethodImpl(MethodImplOptions.NoInlining)] static void Thrower(string m) { Throws(m); }
  public static string NativeDirect(int n) {
    // Several rounds: a method is compiled once it has been called, so the first round runs interpreted.
    long r = 0;
    for (int k = 0; k < 4; k++) r += NatLoop(n);
    string t = "";
    for (int k = 0; k < 3; k++)
      try { Thrower("directly"); t += " no throw"; } catch (ArgumentException e) { t += " caught " + e.Message; }
    return "native " + r + t;
  }
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
