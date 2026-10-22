// Function pointers across an item-3 library's boundary: an LP64 wasm library
// under an aarch64 guest program, the arm64
// counterpart of Wine's ICALL pass (tools/wine-wasm/icall/GexIcall.cpp).
//
// Exits. A function pointer the library calls may be the guest's (a
// callback it was given: sqlite3_exec's, a vfs's or a virtual table
// module's methods). Every indirect call becomes
//
//   __a64_set_icall_target(p)
//   callee = p < 0x3f0000 ? p : __a64x.<sig>
//   call callee(args...)
//
// A pointer below 0x3f0000 is a function-table index (the library's own
// code), called directly; anything else is a guest address. The exit stub,
// one per signature (linkonce_odr), lays the arguments out as AAPCS64 does
// (integers and pointers in x0-x7, floating point in v0-v7, the rest in
// 8-byte stack slots) and calls __a64_guest_call (a64call.c), which runs the
// guest function on the calling thread and hands back x0 (and v0).
//
// Entries. A function of the library whose address is taken may reach the
// guest (sqlite3_vfs_find's vfs, whose methods the shell's appendvfs
// calls). For each, `__a64e.<name>`, a gex F_body that reads the arguments
// from the guest's registers and stack, calls it and returns to x30 with
// the result in x0 or v0; the pair { function, thunk } goes in the data
// section `gexa_entries` (a64call.c exports its bounds as __gexa_entries).
// The shell (cgen/libprov.rs) binds each thunk at its function's table
// index as a guest address, so a guest call to that pointer runs it.
//
// Signatures with anything else (byval structs, sret, i128, vectors) are
// left alone: a call through such a pointer is the library's own, and such
// a function gets no entry thunk. Variadic calls are left alone too.
// GEXA_ICALL_STATS=1 prints the counts.
#include "llvm/IR/IRBuilder.h"
#include "llvm/IR/Module.h"
#include "llvm/IR/PassManager.h"
#include "llvm/Passes/PassBuilder.h"
#include "llvm/Plugins/PassPlugin.h"
#include "llvm/Support/raw_ostream.h"
#include "llvm/Transforms/Utils/ModuleUtils.h"
#include <cstdlib>

using namespace llvm;

namespace {

constexpr uint64_t ICALL_LIMIT = 0x3f0000;

enum Cls { BAD, INT, FP };
Cls cls(Type *T) {
  if (T->isPointerTy() || (T->isIntegerTy() && T->getIntegerBitWidth() <= 64)) return INT;
  if (T->isFloatTy() || T->isDoubleTy()) return FP;
  return BAD;
}

// Whether a call or function of type FT with these attributes can cross.
bool crosses(FunctionType *FT, AttributeList AL) {
  if (FT->isVarArg()) return false;
  Type *R = FT->getReturnType();
  if (!R->isVoidTy() && cls(R) == BAD) return false;
  for (unsigned i = 0; i < FT->getNumParams(); i++) {
    if (cls(FT->getParamType(i)) == BAD) return false;
    if (AL.hasParamAttr(i, Attribute::ByVal) || AL.hasParamAttr(i, Attribute::StructRet) ||
        AL.hasParamAttr(i, Attribute::InAlloca) || AL.hasParamAttr(i, Attribute::Preallocated))
      return false;
  }
  return true;
}

std::string sigName(FunctionType *FT) {
  auto t = [&](Type *T) -> std::string {
    if (T->isVoidTy()) return "v";
    if (T->isPointerTy()) return "p";
    if (T->isFloatTy()) return "f";
    if (T->isDoubleTy()) return "d";
    return "i" + std::to_string(T->getIntegerBitWidth());
  };
  std::string s = "__a64x." + t(FT->getReturnType()) + "_";
  for (Type *P : FT->params()) s += t(P);
  return s;
}

struct GexA64Icall : PassInfoMixin<GexA64Icall> {
  static Value *toBits(IRBuilder<> &B, Value *A, bool sext) {
    Type *T = A->getType(), *I64 = B.getInt64Ty();
    if (T->isPointerTy()) return B.CreatePtrToInt(A, I64);
    if (T->isDoubleTy()) return B.CreateBitCast(A, I64);
    if (T->isFloatTy()) return B.CreateZExt(B.CreateBitCast(A, B.getInt32Ty()), I64);
    return sext ? B.CreateSExtOrTrunc(A, I64) : B.CreateZExtOrTrunc(A, I64);
  }
  static Value *fromBits(IRBuilder<> &B, Value *V, Type *T) {
    if (T->isPointerTy()) return B.CreateIntToPtr(V, T);
    if (T->isDoubleTy()) return B.CreateBitCast(V, T);
    if (T->isFloatTy()) return B.CreateBitCast(B.CreateTrunc(V, B.getInt32Ty()), T);
    return B.CreateTrunc(V, T);
  }

  Function *exitStub(Module &M, CallBase &CB) {
    FunctionType *FT = CB.getFunctionType();
    std::string name = sigName(FT);
    if (Function *F = M.getFunction(name)) return F;
    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C), *I64 = Type::getInt64Ty(C), *Ptr = PointerType::get(C, 0);
    Function *F = Function::Create(FT, GlobalValue::LinkOnceODRLinkage, name, M);
    F->setVisibility(GlobalValue::HiddenVisibility);
    F->addFnAttr(Attribute::NoInline);
    F->addFnAttr("gexa-no-icall");
    IRBuilder<> B(BasicBlock::Create(C, "entry", F));
    unsigned n = FT->getNumParams() ? FT->getNumParams() : 1;
    Value *X = B.CreateAlloca(I64, B.getInt32(8));
    Value *V = B.CreateAlloca(I64, B.getInt32(8));
    Value *S = B.CreateAlloca(I64, B.getInt32(n));
    Value *V0 = B.CreateAlloca(I64);
    unsigned nx = 0, nv = 0, ns = 0;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Argument *A = F->getArg(i);
      Value *W = toBits(B, A, CB.paramHasAttr(i, Attribute::SExt));
      Value *Slot;
      if (cls(A->getType()) == INT && nx < 8) Slot = B.CreateGEP(I64, X, B.getInt32(nx++));
      else if (cls(A->getType()) == FP && nv < 8) Slot = B.CreateGEP(I64, V, B.getInt32(nv++));
      else Slot = B.CreateGEP(I64, S, B.getInt32(ns++));
      B.CreateStore(W, Slot);
    }
    FunctionCallee Call = M.getOrInsertFunction(
        "__a64_guest_call", FunctionType::get(I64, {Ptr, I32, Ptr, I32, Ptr, I32, I64, Ptr}, false));
    Value *R = B.CreateCall(Call, {X, B.getInt32(nx), V, B.getInt32(nv), S, B.getInt32(ns), B.getInt64(0), V0});
    Type *RT = FT->getReturnType();
    if (RT->isVoidTy()) B.CreateRetVoid();
    else if (cls(RT) == FP) B.CreateRet(fromBits(B, B.CreateLoad(I64, V0), RT));
    else B.CreateRet(fromBits(B, R, RT));
    return F;
  }

  static bool escapes(Function &F) {
    for (const Use &U : F.uses()) {
      const User *Us = U.getUser();
      if (auto *CB = dyn_cast<CallBase>(Us))
        if (CB->isCallee(&U)) continue;
      if (auto *GV = dyn_cast<GlobalVariable>(Us))
        if (GV->getSection() == "llvm.metadata") continue;
      if (isa<Constant>(Us)) {
        bool meta = true;
        SmallVector<const User *, 8> work{Us};
        while (!work.empty() && meta) {
          const User *X = work.pop_back_val();
          if (auto *G = dyn_cast<GlobalVariable>(X)) {
            if (G->getSection() != "llvm.metadata" && G->getName() != "llvm.used" &&
                G->getName() != "llvm.compiler.used")
              meta = false;
            continue;
          }
          if (!isa<Constant>(X)) { meta = false; break; }
          for (const User *Y : X->users()) work.push_back(Y);
        }
        if (meta) continue;
      }
      return true;
    }
    return false;
  }

  Function *entryThunk(Module &M, Function &F) {
    if (!crosses(F.getFunctionType(), F.getAttributes())) return nullptr;
    LLVMContext &C = M.getContext();
    Type *I32 = Type::getInt32Ty(C), *I64 = Type::getInt64Ty(C);
    FunctionType *FT = F.getFunctionType();
    FunctionType *BodyT = FunctionType::get(I32, {I32, I32, I32, I64}, false);
    Function *T = Function::Create(BodyT, GlobalValue::InternalLinkage, "__a64e." + F.getName(), M);
    T->addFnAttr("gexa-no-icall");
    IRBuilder<> B(BasicBlock::Create(C, "entry", T));
    Value *Ctx = T->getArg(0), *Depth = T->getArg(1), *Ret = T->getArg(3);
    auto helper = [&](const char *n, Type *R, ArrayRef<Type *> P) {
      return M.getOrInsertFunction(n, FunctionType::get(R, P, false));
    };
    unsigned nx = 0, nv = 0, off = 0;
    std::vector<Value *> args;
    for (unsigned i = 0; i < FT->getNumParams(); i++) {
      Type *P = FT->getParamType(i);
      Value *W;
      if (cls(P) == INT && nx < 8)
        W = B.CreateCall(helper("__a64_reg", I64, {I32, I32}), {Ctx, B.getInt32(nx++)});
      else if (cls(P) == FP && nv < 8)
        W = B.CreateCall(helper("__a64_vreg", I64, {I32, I32}), {Ctx, B.getInt32(nv++)});
      else {
        W = B.CreateCall(helper("__a64_stack", I64, {I32, I32}), {Ctx, B.getInt32(off)});
        off += 8;
      }
      args.push_back(fromBits(B, W, P));
    }
    // after the arguments are read (a thread's first entry may make syscalls)
    B.CreateCall(helper("__a64_entry", Type::getVoidTy(C), {I32, I32}), {Ctx, Depth});
    Value *R = B.CreateCall(FT, &F, args);
    Type *RT = FT->getReturnType();
    Value *Out;
    if (RT->isVoidTy()) {
      Value *X0 = B.CreateCall(helper("__a64_reg", I64, {I32, I32}), {Ctx, B.getInt32(0)});
      Out = B.CreateCall(helper("__a64_ret", I32, {I32, I64, I64}), {Ctx, Ret, X0});
    } else if (cls(RT) == FP) {
      Out = B.CreateCall(helper("__a64_ret_v", I32, {I32, I64, I64}), {Ctx, Ret, toBits(B, R, false)});
    } else {
      bool sext = F.getAttributes().hasRetAttr(Attribute::SExt);
      Out = B.CreateCall(helper("__a64_ret", I32, {I32, I64, I64}), {Ctx, Ret, toBits(B, R, sext)});
    }
    B.CreateRet(Out);
    return T;
  }

  unsigned entryThunks(Module &M) {
    std::vector<Function *> fs;
    for (Function &F : M) {
      if (F.isDeclaration() || F.isIntrinsic() || F.hasFnAttribute("gexa-no-icall")) continue;
      StringRef n = F.getName();
      if (n.starts_with("__a64") || n.starts_with("gexp_") || n.starts_with("llvm.")) continue;
      if (escapes(F)) fs.push_back(&F);
    }
    std::vector<std::pair<Function *, Function *>> pairs;
    for (Function *F : fs)
      if (Function *T = entryThunk(M, *F)) pairs.push_back({F, T});
    if (pairs.empty()) return 0;
    LLVMContext &C = M.getContext();
    Type *Ptr = PointerType::get(C, 0);
    StructType *PairT = StructType::get(Ptr, Ptr);
    std::vector<Constant *> elems;
    for (auto &[F, T] : pairs) elems.push_back(ConstantStruct::get(PairT, {F, T}));
    ArrayType *AT = ArrayType::get(PairT, elems.size());
    auto *G = new GlobalVariable(M, AT, true, GlobalValue::InternalLinkage, ConstantArray::get(AT, elems), "__a64e.pairs");
    G->setSection("gexa_entries");
    G->setAlignment(Align(8));
    appendToUsed(M, {G});
    return pairs.size();
  }

  PreservedAnalyses run(Module &M, ModuleAnalysisManager &) {
    LLVMContext &C = M.getContext();
    Type *I64 = Type::getInt64Ty(C);
    unsigned thunks = getenv("GEXA_NO_ENTRY_THUNKS") ? 0 : entryThunks(M);
    FunctionType *SetT = FunctionType::get(Type::getVoidTy(C), {I64}, false);
    unsigned done = 0, left = 0;
    std::vector<CallBase *> sites;
    for (Function &F : M) {
      if (F.isDeclaration() || F.hasFnAttribute("gexa-no-icall")) continue;
      for (BasicBlock &BB : F)
        for (Instruction &I : BB)
          if (auto *CB = dyn_cast<CallBase>(&I))
            if (!CB->getCalledFunction() && !CB->isInlineAsm() &&
                !isa<Function>(CB->getCalledOperand()->stripPointerCasts()))
              sites.push_back(CB);
    }
    for (CallBase *CB : sites) {
      if (!isa<CallInst>(CB) || !crosses(CB->getFunctionType(), CB->getAttributes())) {
        left++;
        continue;
      }
      Function *Stub = exitStub(M, *CB);
      IRBuilder<> B(CB);
      Value *Callee = CB->getCalledOperand();
      Value *P = B.CreatePtrToInt(Callee, I64);
      B.CreateCall(M.getOrInsertFunction("__a64_set_icall_target", SetT), {P});
      Value *Native = B.CreateICmpULT(P, B.getInt64(ICALL_LIMIT));
      CB->setCalledOperand(B.CreateSelect(Native, Callee, Stub));
      done++;
    }
    if (getenv("GEXA_ICALL_STATS"))
      errs() << "gex a64 icall: " << M.getName() << ": " << done << " sites, " << left << " left alone, "
             << thunks << " entry thunks\n";
    return done || thunks ? PreservedAnalyses::none() : PreservedAnalyses::all();
  }
};

} // namespace

extern "C" LLVM_ATTRIBUTE_WEAK PassPluginLibraryInfo llvmGetPassPluginInfo() {
  return {LLVM_PLUGIN_API_VERSION, "GexA64Icall", "0.1", [](PassBuilder &PB) {
            PB.registerOptimizerLastEPCallback(
                [](ModulePassManager &MPM, OptimizationLevel, ThinOrFullLTOPhase) { MPM.addPass(GexA64Icall()); });
          }};
}
