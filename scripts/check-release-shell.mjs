import fs from 'node:fs'
import { spawnSync } from 'node:child_process'

// Check the literal Bash blocks as GitHub will indent them. In particular,
// here-document terminators must start at column zero after YAML indentation
// is removed; actionlint without ShellCheck does not catch this.
const lines = fs.readFileSync(process.argv[2] ?? '.github/workflows/release.yml', 'utf8').split('\n')
let checked = 0
for (let i = 0; i < lines.length; i++) {
  const match = lines[i].match(/^(\s*)run: \|\s*$/)
  if (!match) continue
  const line = i + 1, indentation = match[1].length + 2, body = []
  while (++i < lines.length) {
    if (lines[i].trim() && lines[i].search(/\S/) < indentation) { i--; break }
    body.push(lines[i].slice(indentation))
  }
  const script = body.join('\n').replace(/\$\{\{[\s\S]*?\}\}/g, 'CI_EXPRESSION')
  const result = spawnSync('bash', ['-n'], { input: script, encoding: 'utf8' })
  if (result.error) throw result.error
  if (result.status !== 0) throw new Error(`release.yml:${line}: ${result.stderr}`)
  checked++
}
console.log(`Checked Bash syntax in ${checked} release workflow blocks.`)
