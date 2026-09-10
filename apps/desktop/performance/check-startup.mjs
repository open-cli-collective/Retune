// Check browser resource observations, including Vite's conditional-preload regression.
import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
const report = JSON.parse(await readFile(process.argv[2], 'utf8'))
assert.equal(report.commit, process.argv[3], 'Use observations from the candidate being reviewed')
for (const label of ['main', 'lastfm-importer']) {
  const samples = report.samples.filter(sample => sample.window === label)
  assert(samples.length >= 3)
  for (const sample of samples) {
    const unwanted = label === 'main' ? 'LastFmImporter-' : 'App-'
    assert(!sample.resources.some(resource => resource.name.startsWith(unwanted)), `${label} loaded the other window`)
    if (label === 'lastfm-importer') assert(sample.resources.some(resource => resource.name.startsWith('LastFmImporter-') && resource.name.endsWith('.css')), 'Importer CSS was not loaded')
  }
}
console.log('Both window entries load only their own view and required importer CSS')
