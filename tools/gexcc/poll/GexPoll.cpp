// GexPoll: safepoint polls in native wasm32 code (docs/gexcc.md §3).
//
// gex delivers a signal to a native program at a system call's return
// (docs/wine-native.md §3.11): it cannot interrupt running wasm. A loop that
// makes no system calls (a computation, a busy wait, a test that relies on
// alarm() to stop it) would never see SIGALRM, SIGINT or SIGTERM. Translated
// code polls at back-edges (tier-1's safepoint countdown); this pass gives
// gexcc-built code the same: at every loop header
//
//     if (--__gex_poll_counter < 0) __gex_poll();
//
// __gex_poll_counter is a wasm global, and so per thread (one module
// instance per thread). __gex_poll (tools/gexcc/rt/poll.c) re-arms it and
// makes one cheap system call, at whose return any pending signal is
// delivered and the kernel's interval timers are checked.
//
// Run at the end of the optimisation pipeline (after inlining and loop
// transforms), as GexIcall is: -fpass-plugin=GexPoll.so.
#include "llvm/Analysis/LoopInfo.h"
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/MDBuilder.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"
#include "llvm/Transforms/Utils/BasicBlockUtils.h"

using namespace llvm;

namespace {

struct GexPoll : PassInfoMixin<GexPoll> {
  PreservedAnalyses run(Module &M, ModuleAnalysisManager &MAM) {
    auto &FAM = MAM.getResult<FunctionAnalysisManagerModuleProxy>(M).getManager();
    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C);
    // the wasm global (address space 1), defined in rt/poll.s
    GlobalVariable *Ctr = M.getNamedGlobal("__gex_poll_counter");
    FunctionCallee Poll;
    bool changed = false;
    for (Function &F : M) {
      if (F.isDeclaration() || F.getName().starts_with("__gex_"))
        continue;
      LoopInfo &LI = FAM.getResult<LoopAnalysis>(F);
      SmallVector<BasicBlock *, 8> headers;
      for (Loop *L : LI.getLoopsInPreorder())
        headers.push_back(L->getHeader());
      if (headers.empty())
        continue;
      if (!Ctr) {
        Ctr = new GlobalVariable(M, I32, false, GlobalValue::ExternalLinkage, nullptr, "__gex_poll_counter",
                                 nullptr, GlobalValue::NotThreadLocal, 1);
        Poll = M.getOrInsertFunction("__gex_poll", FunctionType::get(Type::getVoidTy(C), false));
        if (auto *PF = dyn_cast<Function>(Poll.getCallee()))
          PF->addFnAttr(Attribute::Cold);
      } else if (!Poll) {
        Poll = M.getOrInsertFunction("__gex_poll", FunctionType::get(Type::getVoidTy(C), false));
      }
      for (BasicBlock *H : headers) {
        Instruction *At = &*H->getFirstInsertionPt();
        IRBuilder<> B(At);
        Value *V = B.CreateLoad(I32, Ctr, "gexpoll");
        Value *N = B.CreateSub(V, ConstantInt::get(I32, 1));
        B.CreateStore(N, Ctr);
        Value *Due = B.CreateICmpSLT(N, ConstantInt::get(I32, 0));
        MDNode *W = MDBuilder(C).createBranchWeights(1, 1 << 20);
        Instruction *Then = SplitBlockAndInsertIfThen(Due, At, false, W);
        IRBuilder<> T(Then);
        T.CreateCall(Poll);
      }
      changed = true;
      FAM.invalidate(F, PreservedAnalyses::none());
    }
    return changed ? PreservedAnalyses::none() : PreservedAnalyses::all();
  }
};

} // namespace

extern "C" LLVM_ATTRIBUTE_WEAK PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "GexPoll", "0.1", [](PassBuilder &PB) {
            PB.registerOptimizerLastEPCallback(
                [](ModulePassManager &MPM, OptimizationLevel, ThinOrFullLTOPhase) { MPM.addPass(GexPoll()); });
          }};
}
