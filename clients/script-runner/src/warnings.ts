import ts from "typescript";

function syntax(source: string) {
  const fileName = "bundle.js";
  const file = ts.createSourceFile(fileName, source, ts.ScriptTarget.Latest, true, ts.ScriptKind.JS);
  const host: ts.CompilerHost = {
    getSourceFile: name => name === fileName ? file : undefined,
    getDefaultLibFileName: () => "", writeFile: () => {}, getCurrentDirectory: () => "",
    getDirectories: () => [], fileExists: name => name === fileName,
    readFile: name => name === fileName ? source : undefined,
    getCanonicalFileName: name => name, useCaseSensitiveFileNames: () => true, getNewLine: () => "\n",
  };
  const program = ts.createProgram([fileName], { allowJs: true, noLib: true, noResolve: true }, host);
  const checker = program.getTypeChecker();
  return { file, checker };
}

export function literalRequires(source: string): string[] {
  const { file, checker } = syntax(source);
  const imports: string[] = [];
  function isRequire(node: ts.Expression, seen = new Set<ts.Symbol>()): boolean {
    if (ts.isPropertyAccessExpression(node) && node.name.text === "require" &&
        ts.isMetaProperty(node.expression) && node.expression.keywordToken === ts.SyntaxKind.ImportKeyword) return true;
    if (!ts.isIdentifier(node)) return false;
    const symbol = checker.getSymbolAtLocation(node);
    if (!symbol) return node.text === "require";
    if (seen.has(symbol)) return false;
    seen.add(symbol);
    const declaration = symbol.valueDeclaration;
    return !!declaration && ts.isVariableDeclaration(declaration) && !!declaration.initializer &&
      isRequire(declaration.initializer, seen);
  }
  function visit(node: ts.Node): void {
    if (ts.isCallExpression(node) && isRequire(node.expression)) {
      const arg = node.arguments[0];
      if (arg && (ts.isStringLiteral(arg) || ts.isNoSubstitutionTemplateLiteral(arg))) imports.push(arg.text);
    }
    ts.forEachChild(node, visit);
  }
  visit(file);
  return imports;
}

export function pathWarnings(source: string): string[] {
  const { file, checker } = syntax(source);
  const warnings: string[] = [];
  function visit(node: ts.Node): void {
    if (ts.isIdentifier(node) && ["__dirname", "__filename"].includes(node.text)) {
      const parent = node.parent;
      const propertyName = (ts.isPropertyAccessExpression(parent) && parent.name === node) ||
        (ts.isPropertyAssignment(parent) && parent.name === node) ||
        (ts.isBindingElement(parent) && parent.propertyName === node);
      const symbol = ts.isShorthandPropertyAssignment(parent)
        ? checker.getShorthandAssignmentValueSymbol(parent) : checker.getSymbolAtLocation(node);
      if (!propertyName && !symbol) {
        const line = file.getLineAndCharacterOfPosition(node.getStart(file)).line + 1;
        warnings.push(`script_path_warning: ${node.text} at line ${line}: no script folder; paths resolve against the working directory`);
      }
    }
    ts.forEachChild(node, visit);
  }
  visit(file);
  return warnings;
}
