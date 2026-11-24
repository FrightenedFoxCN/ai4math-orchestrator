set_option debug.skipKernelTC false
set_option Elab.async false

-- Do not elaborate a user name or statement here. Construct the type over the
-- authenticated kernel declaration and the kernel's False constant directly.
private def ai4mathSealedWitnessType (env : Lean.Environment) : Except String Lean.Expr := do
  let some home := env.getModuleIdx? ai4mathSealedModule
    | throw "missing sealed proposition module"
  unless env.getModuleIdxFor? ai4mathSealedProposition == some home do
    throw "sealed proposition is not declared by the committed target module"
  let some (.defnInfo proposition) := env.find? ai4mathSealedProposition
    | throw "sealed proposition must be a public definition of Prop"
  unless (match proposition.safety with | .safe => true | _ => false) do
    throw "sealed proposition must be a safe definition"
  unless proposition.levelParams.isEmpty && proposition.type == .sort .zero &&
      !proposition.value.hasLooseBVars && !proposition.value.hasFVar && !proposition.value.hasMVar do
    throw "sealed proposition must be a closed, monomorphic definition of Prop"
  return .forallE `h (.const ai4mathSealedProposition []) (.const `False []) .default

open Lean Elab Term in
elab "ai4mathSealedRefutation" : term => do
  match ai4mathSealedWitnessType (← getEnv) with
  | .ok type => return type
  | .error reason => throwError reason
