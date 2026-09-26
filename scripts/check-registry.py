import json, os, sys
data = json.load(open('registry.json'))
meta = json.load(open('scripts/registry-meta.json'))
assert 'meta' in data, 'missing meta'
assert 'apiVersion' in data['meta'], 'missing meta.apiVersion'
assert isinstance(data.get('revoked', []), list), 'revoked must be list'
count = 0
for slug, entry in data.items():
    if slug in ('meta', 'revoked'):
        continue
    assert 'name' in entry, slug + ' missing name'
    assert 'versions' in entry, slug + ' missing versions'
    assert entry['versions'], slug + ' empty versions'
    assert entry.get('tags') != ['stable'], slug + ' tags look like a leaked status column'
    assert slug not in meta, slug + ' is released — remove it from scripts/registry-meta.json'
    count += 1
for slug, entry in meta.items():
    if slug.startswith('_'):
        continue
    for k in ('name', 'description', 'category', 'tags', 'status'):
        assert entry.get(k), slug + ' registry-meta.json missing ' + k
# Every shipped plugin must be installable or queued for its first release.
for slug in sorted(os.listdir('plugins')):
    if os.path.isfile(os.path.join('plugins', slug, 'plugin.json')):
        assert slug in data or slug in meta, 'plugins/%s is in neither registry.json nor scripts/registry-meta.json' % slug
print('registry.json: %d plugins validated, %d pending first release' % (count, len([s for s in meta if not s.startswith('_')])))
