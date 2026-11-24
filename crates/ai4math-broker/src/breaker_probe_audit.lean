import Lean
import Lean.Replay

open Lean

private def exactName (text : String) : Name :=
  (text.splitOn ".").foldl Name.str .anonymous

private def moduleOf (env : Environment) (name : Name) : Option Name := do
  let idx ← env.getModuleIdxFor? name
  env.header.moduleNames[idx.toNat]?

private def requireHome (env : Environment) (mod name : Name) : IO Unit := do
  unless !isPrivateName name && moduleOf env name == some mod do
    throw <| IO.userError s!"'{name}' is not a public declaration of the committed target module"

private def mentions (name : Name) (type : Expr) : Bool :=
  (type.find? fun
    | .const n _ => n == name
    | _ => false).isSome

-- Replay only the roots and their dependencies. The installed Lean environment
-- is imported before the committed objects go on the search path. Nothing in a
-- committed module may replace a trusted declaration in the replay environment.
private partial def replayClosure (trusted env : Environment) (todo : List Name)
    (acc : Std.HashMap Name ConstantInfo := {}) : IO (Std.HashMap Name ConstantInfo) := do
  match todo with
  | [] => return acc
  | name :: rest =>
    if acc.contains name then
      replayClosure trusted env rest acc
    else if (trusted.find? name).isSome then
      unless moduleOf env name == moduleOf trusted name do
        throw <| IO.userError s!"committed declaration shadows installed Lean constant '{name}'"
      replayClosure trusted env rest acc
    else
      let some info := env.find? name
        | throw <| IO.userError s!"missing kernel declaration '{name}'"
      if info.isUnsafe || info.isPartial then
        throw <| IO.userError s!"unsafe or partial dependency '{name}'"
      if let .axiomInfo _ := info then
        throw <| IO.userError s!"non-kernel axiom '{name}'"
      let deps := info.getUsedConstantsAsSet.toList ++ (match info with
        | .inductInfo v => v.all
        | .ctorInfo v => [v.induct]
        | _ => [])
      replayClosure trusted env (deps ++ rest) (acc.insert name info)

private def auditAxioms (env : Environment) (name : Name) : IO Unit := do
  let check : CoreM (Array Name) := Lean.collectAxioms name
  let (axioms, _) ← check.toIO { fileName := "<breaker probe>", fileMap := default } { env }
  for ax in axioms do
    unless ax == `propext || ax == `Classical.choice || ax == `Quot.sound do
      throw <| IO.userError s!"'{name}' depends on non-kernel axiom '{ax}'"

def main (args : List String) : IO Unit := do
  let objects :: moduleText :: defText :: caseTexts := args
    | throw <| IO.userError "missing breaker audit arguments"
  unless caseTexts.length == 1 || (caseTexts.length == 2 && caseTexts[0]? != caseTexts[1]?) do
    throw <| IO.userError "expected one or two distinct breaker cases"
  let mod := Name.mkSimple moduleText
  let watched := Name.mkSimple defText
  let cases := caseTexts.map exactName
  let builtins : Array Import := #[{ module := `Lean }]
  let trusted ← Lean.importModules builtins {} (loadExts := false)

  -- Temporary object files are data only: no imported parser, elaborator,
  -- environment extension, or initializer can participate in this checker.
  let builtinPath ← Lean.searchPathRef.get
  Lean.searchPathRef.set (builtinPath ++ [System.FilePath.mk objects])
  let env ← Lean.importModules (builtins.push { module := mod }) {} (loadExts := false)
  requireHome env mod watched
  let some (.defnInfo defn) := env.find? watched
    | throw <| IO.userError "watched declaration must be a public definition"
  unless (match defn.safety with | .safe => true | _ => false) do
    throw <| IO.userError "watched declaration must be a safe definition"
  for case in cases do
    requireHome env mod case
    let some (.thmInfo thm) := env.find? case
      | throw <| IO.userError s!"'{case}' must be a theorem"
    unless mentions watched thm.type do
      throw <| IO.userError s!"statement of '{case}' does not directly reference '{watched}'"

  -- Object imports alone trust pre-checked declarations. Replay the audited
  -- closure through the native kernel rather than trusting source options.
  let roots := watched :: cases
  let closure ← replayClosure trusted env roots
  let checked ← trusted.toKernelEnv.replay closure
  for name in roots do
    auditAxioms (.ofKernelEnv checked) name
  IO.println "ai4math:sealed-breaker-probe-audit:v1"
