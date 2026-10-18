// A .NET program on W3 (dotnet.sh): mscoree loads the dist's Wine Mono,
// which JIT-compiles this and runs it. Prints "dotnet ok" and the runtime.
using System;
class Hello {
    static int Main() {
        int sum = 0;
        for (int i = 1; i <= 100; i++) sum += i;
        if (sum != 5050) { Console.WriteLine("dotnet: wrong sum " + sum); return 1; }
        Console.WriteLine("dotnet ok (CLR " + Environment.Version + ")");
        return 0;
    }
}
