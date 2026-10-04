using Mono.Cecil;
using Mono.Cecil.Cil;
// usage: apiscan GAME_MANAGED NETFW [verbose]
var game = args[0]; var fw = args[1]; bool verbose = args.Length > 2;
var fwNames = Directory.GetFiles(fw, "*.dll").Select(Path.GetFileNameWithoutExtension).ToHashSet(StringComparer.OrdinalIgnoreCase);
var res = new Res(game, fw, fwNames);
var rp = new ReaderParameters { AssemblyResolver = res };
var replaced = new List<string>(); var scan = new List<string>();
foreach (var f in Directory.GetFiles(game, "*.dll").OrderBy(x => x)) {
  var n = Path.GetFileNameWithoutExtension(f);
  if (fwNames.Contains(n)) replaced.Add(n); else scan.Add(f);
}
Console.WriteLine($"replaced by .NET ({replaced.Count}): {string.Join(' ', replaced)}");
var missingByAsm = new SortedDictionary<string, SortedSet<string>>();
int totalRefs = 0, totalMiss = 0;
foreach (var f in scan) {
  AssemblyDefinition a;
  try { a = res.Load(Path.GetFileNameWithoutExtension(f)); } catch (Exception e) { Console.WriteLine($"!! {f}: {e.Message}"); continue; }
  var m = a.MainModule; var miss = new SortedSet<string>();
  int icalls = 0, pinv = 0; var pmods = new SortedSet<string>();
  foreach (var t in m.GetTypes()) foreach (var md in t.Methods) {
    if (md.IsInternalCall) icalls++;
    if (md.IsPInvokeImpl && md.PInvokeInfo != null) { pinv++; pmods.Add(md.PInvokeInfo.Module.Name); }
  }
  foreach (var tr in m.GetTypeReferences()) {
    totalRefs++;
    if (tr.Scope is AssemblyNameReference anr && !fwNames.Contains(anr.Name)) continue; // game-local
    TypeDefinition td = null; try { td = tr.Resolve(); } catch (Exception) {}
    if (td == null) { miss.Add("T " + tr.FullName + " [" + tr.Scope?.Name + "]"); }
  }
  foreach (var mr in m.GetMemberReferences()) {
    totalRefs++;
    var dt = mr.DeclaringType; if (dt is ArrayType) continue; var et = dt.GetElementType();
    if (et.Scope is AssemblyNameReference anr && !fwNames.Contains(anr.Name)) continue;
    if (et.Scope is ModuleDefinition) continue;
    IMemberDefinition d = null; try { d = mr.Resolve(); } catch (Exception) {}
    if (d == null) miss.Add("M " + mr.FullName + " [" + et.Scope?.Name + "]");
  }
  totalMiss += miss.Count;
  var refs = string.Join(",", m.AssemblyReferences.Select(r => r.Name + "/" + r.Version.Major));
  Console.WriteLine($"== {Path.GetFileName(f)}: icalls={icalls} pinvoke={pinv} [{string.Join(",", pmods)}] missing={miss.Count}");
  if (verbose) Console.WriteLine("   refs: " + refs);
  foreach (var s in miss) Console.WriteLine("   " + s);
}
Console.WriteLine($"TOTAL refs={totalRefs} missing={totalMiss}");

class Res : BaseAssemblyResolver {
  string game, fw; HashSet<string> fwNames; Dictionary<string, AssemblyDefinition> cache = new(StringComparer.OrdinalIgnoreCase);
  public Res(string g, string f, HashSet<string> n) { game = g; fw = f; fwNames = n; }
  public AssemblyDefinition Load(string name) {
    if (cache.TryGetValue(name, out var a)) return a;
    string p = fwNames.Contains(name) ? Path.Combine(fw, name + ".dll") : Path.Combine(game, name + ".dll");
    if (!File.Exists(p)) return null;
    a = AssemblyDefinition.ReadAssembly(p, new ReaderParameters { AssemblyResolver = this });
    cache[name] = a; return a;
  }
  public override AssemblyDefinition Resolve(AssemblyNameReference n) {
    var a = Load(n.Name); if (a == null) throw new AssemblyResolutionException(n); return a;
  }
}
