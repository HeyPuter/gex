// The ICALL pass (docs/wine-native.md §3.3): native Wine code calling
// through a function pointer may be calling application (guest x86) code.
//
// Every indirect call becomes the select form measured in W0(c):
//
//   gexw_set_icall_target(p)
//   callee = p < 0x400000 ? p : __gexw_exit.<sig>
//   call callee(args...)
//
// A function pointer below 0x400000 is a function-table index (native
// code), called directly; anything else is a guest address. The exit stub,
// one per wasm signature (linkonce_odr), lays the arguments out as i386
// stack words and calls __gexw_guest_call / __gexw_guest_call_fp
// (gexw_rt.c), which run the guest function through gex's function table
// and hand back EDX:EAX or ST0.
//
// Build: tools/wine-wasm/icall/build.sh. Use: clang -fpass-plugin=GexIcall.so.
// Variadic indirect calls are left alone (counted; GEXW_ICALL_STATS=1).
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"
#include "llvm/Support/raw_ostream.h"
#include <cstdlib>

using namespace llvm;

namespace {

constexpr uint64_t ICALL_LIMIT = 0x400000;

std::string sigName(FunctionType *FT, const DataLayout &DL) {
  auto t = [&](Type *T) -> std::string {
    if (T->isVoidTy()) return "v";
    if (T->isPointerTy()) return "p";
    if (T->isFloatTy()) return "f";
    if (T->isDoubleTy()) return "d";
    if (T->isIntegerTy()) return "i" + std::to_string(T->getIntegerBitWidth());
    std::string s;
    raw_string_ostream o(s);
    T->print(o);
    return "x" + std::to_string(std::hash<std::string>()(o.str()) & 0xffff);
  };
  std::string s = "__gexw_exit." + t(FT->getReturnType()) + "_";
  for (Type *P : FT->params()) s += t(P);
  return s;
}

struct GexIcall : PassInfoMixin<GexIcall> {
  Function *getTarget(Module &M, const char *name, FunctionType *FT) {
    return cast<Function>(M.getOrInsertFunction(name, FT).getCallee());
  }

  // The exit stub for calls of type FT (byval attributes are taken from the
  // first call site seen; they are part of the C type in practice).
  Function *exitStub(Module &M, CallBase &CB) {
    FunctionType *FT = CB.getFunctionType();
    const DataLayout &DL = M.getDataLayout();
    std::string name = sigName(FT, DL);
    // byval parameters change the stack layout: give them their own stubs.
    for (unsigned i = 0; i < FT->getNumParams(); i++)
      if (Type *BT = CB.getParamByValType(i))
        name += ".bv" + std::to_string(i) + "_" + std::to_string(DL.getTypeAllocSize(BT));
    if (Function *F = M.getFunction(name)) return F;

    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C), *I64 = Type::getInt64Ty(C);
    Type *Ptr = PointerType::get(C, 0);
    Function *F = Function::Create(FT, GlobalValue::LinkOnceODRLinkage, name, M);
    F->setVisibility(GlobalValue::HiddenVisibility);
    F->addFnAttr(Attribute::NoInline);
    F->addFnAttr("gexw-no-icall");
    BasicBlock *BB = BasicBlock::Create(C, "entry", F);
    IRBuilder<> B(BB);

    // Count the i386 stack words.
    unsigned words = 0;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Type *P = FT->getParamType(i);
      if (Type *BT = CB.getParamByValType(i)) words += (DL.getTypeAllocSize(BT) + 3) / 4;
      else if (P->isIntegerTy(64) || P->isDoubleTy()) words += 2;
      else words += 1;
    }
    Value *Buf = B.CreateAlloca(I32, B.getInt32(words ? words : 1));
    unsigned w = 0;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Argument *A = F->getArg(i);
      Type *P = A->getType();
      auto slot = [&](unsigned k) { return B.CreateGEP(I32, Buf, B.getInt32(k)); };
      if (Type *BT = CB.getParamByValType(i)) {
        uint64_t sz = DL.getTypeAllocSize(BT);
        B.CreateMemCpy(slot(w), Align(4), A, Align(1), sz);
        w += (sz + 3) / 4;
      } else if (P->isIntegerTy(64) || P->isDoubleTy()) {
        Value *V = P->isDoubleTy() ? B.CreateBitCast(A, I64) : (Value *)A;
        B.CreateStore(B.CreateTrunc(V, I32), slot(w));
        B.CreateStore(B.CreateTrunc(B.CreateLShr(V, 32), I32), slot(w + 1));
        w += 2;
      } else if (P->isPointerTy()) {
        B.CreateStore(B.CreatePtrToInt(A, I32), slot(w++));
      } else if (P->isFloatTy()) {
        B.CreateStore(B.CreateBitCast(A, I32), slot(w++));
      } else if (P->isIntegerTy()) {
        // i386 passes narrow integers widened to a word.
        B.CreateStore(B.CreateZExtOrTrunc(A, I32), slot(w++));
      } else {
        report_fatal_error("gex icall: unsupported parameter type in " + Twine(name));
      }
    }
    Function *Get = getTarget(M, "gexw_get_icall_target", FunctionType::get(I32, false));
    Value *T = B.CreateCall(Get);
    Type *R = FT->getReturnType();
    if (R->isFloatingPointTy()) {
      Function *Call = getTarget(M, "__gexw_guest_call_fp", FunctionType::get(B.getDoubleTy(), {I32, Ptr, I32}, false));
      Value *V = B.CreateCall(Call, {T, Buf, B.getInt32(words)});
      B.CreateRet(R->isFloatTy() ? B.CreateFPTrunc(V, R) : V);
    } else {
      Function *Call = getTarget(M, "__gexw_guest_call", FunctionType::get(I64, {I32, Ptr, I32}, false));
      Value *V = B.CreateCall(Call, {T, Buf, B.getInt32(words)});
      if (R->isVoidTy()) B.CreateRetVoid();
      else if (R->isPointerTy()) B.CreateRet(B.CreateIntToPtr(B.CreateTrunc(V, I32), R));
      else if (R->isIntegerTy()) B.CreateRet(B.CreateTrunc(V, R));
      else report_fatal_error("gex icall: unsupported return type in " + Twine(name));
    }
    return F;
  }

  PreservedAnalyses run(Module &M, ModuleAnalysisManager &) {
    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C);
    FunctionType *SetT = FunctionType::get(Type::getVoidTy(C), {I32}, false);
    unsigned done = 0, varargs = 0;
    std::vector<CallBase *> sites;
    for (Function &F : M) {
      if (F.isDeclaration() || F.hasFnAttribute("gexw-no-icall")) continue;
      for (BasicBlock &BB : F)
        for (Instruction &I : BB)
          if (auto *CB = dyn_cast<CallBase>(&I))
            if (!CB->getCalledFunction() && !CB->isInlineAsm() &&
                !isa<Function>(CB->getCalledOperand()->stripPointerCasts()))
              sites.push_back(CB);
    }
    for (CallBase *CB : sites) {
      if (CB->getFunctionType()->isVarArg()) {
        varargs++;
        continue;
      }
      Function *Stub = exitStub(M, *CB);
      IRBuilder<> B(CB);
      Value *Callee = CB->getCalledOperand();
      Value *P = B.CreatePtrToInt(Callee, I32);
      B.CreateCall(M.getOrInsertFunction("gexw_set_icall_target", SetT), {P});
      Value *Native = B.CreateICmpULT(P, B.getInt32(ICALL_LIMIT));
      CB->setCalledOperand(B.CreateSelect(Native, Callee, Stub));
      done++;
    }
    if (getenv("GEXW_ICALL_STATS"))
      errs() << "gex icall: " << M.getName() << ": " << done << " sites, " << varargs << " variadic left alone\n";
    return done ? PreservedAnalyses::none() : PreservedAnalyses::all();
  }
};

} // namespace

extern "C" LLVM_ATTRIBUTE_WEAK PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "GexIcall", "0.1", [](PassBuilder &PB) {
            PB.registerOptimizerLastEPCallback(
                [](ModulePassManager &MPM, OptimizationLevel, ThinOrFullLTOPhase) { MPM.addPass(GexIcall()); });
          }};
}
