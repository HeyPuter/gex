// GexLLP64: clang's wasm64 target with Windows x64's data model (LLP64), for
// Wine's PE side under 64-bit Wine. clang has no
// wasm64 Windows triple; this plugin changes the data model of the
// wasm64-unknown-unknown target before anything is parsed:
//   long 32 bits; long double = double (MSVC); size_t, ptrdiff_t, intptr_t,
//   intmax_t and int64_t are long long; wint_t is unsigned short.
// The predefined macros are computed before plugins run, so llp64.h
// (mkmacros.sh, force-included) redefines them as x86_64 MSVC's. Use with
// -mms-bitfields and -fshort-wchar. tests/wine64/layout.sh checks the result.
//
//   clang --target=wasm64-unknown-unknown -fplugin=GexLLP64.so -include llp64.h ...
#include "clang/AST/ASTConsumer.h"
#include "clang/Basic/TargetInfo.h"
#include "clang/Frontend/CompilerInstance.h"
#include "clang/Frontend/FrontendPluginRegistry.h"
#include "llvm/ADT/APFloat.h"

using namespace clang;

namespace {

// The integer-type choices are protected members of TransferrableTargetInfo;
// a member pointer formed in a derived class reaches them on any TargetInfo.
struct Model : TargetInfo {
  using TI = TransferrableTargetInfo;
  static void apply(TargetInfo &T) {
    T.LongWidth = T.LongAlign = 32;
    T.LongDoubleWidth = T.LongDoubleAlign = 64;
    T.LongDoubleFormat = &llvm::APFloat::IEEEdouble();
    IntType TI::*ll[] = {&Model::SizeType, &Model::IntMaxType, &Model::PtrDiffType, &Model::IntPtrType,
                         &Model::Int64Type};
    for (auto m : ll)
      T.*m = (m == &Model::SizeType) ? UnsignedLongLong : SignedLongLong;
    T.*(&Model::WIntType) = UnsignedShort;
  }
};

struct Action : PluginASTAction {
  std::unique_ptr<ASTConsumer> CreateASTConsumer(CompilerInstance &, llvm::StringRef) override {
    return std::make_unique<ASTConsumer>();
  }
  bool ParseArgs(const CompilerInstance &CI, const std::vector<std::string> &) override {
    const TargetInfo &T = CI.getTarget();
    if (T.getTriple().getArch() != llvm::Triple::wasm64) {
      CI.getDiagnostics().Report(CI.getDiagnostics().getCustomDiagID(
          DiagnosticsEngine::Error, "GexLLP64: the target must be wasm64"));
      return false;
    }
    Model::apply(const_cast<TargetInfo &>(T));
    return true;
  }
  ActionType getActionType() override { return AddBeforeMainAction; }
};

} // namespace

static FrontendPluginRegistry::Add<Action> X("gex-llp64", "Windows x64 data model (LLP64) on wasm64");
