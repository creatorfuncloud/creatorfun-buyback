"""Build the Anchor 0.31 JSON IDL for creatorfun_buyback straight from lib.rs (no anchor CLI needed)."""
import hashlib, json, re, sys

src = open(sys.argv[1]).read()
out = sys.argv[2]
disc = lambda s: list(hashlib.sha256(s.encode()).digest()[:8])
PRIM = {'u8', 'u16', 'u32', 'u64', 'u128', 'i8', 'i16', 'i32', 'i64', 'i128', 'bool'}

def rtype(t):
    t = t.strip()
    if t == 'Pubkey': return 'pubkey'
    if t in PRIM: return t
    raise SystemExit('unknown type ' + t)

def struct_fields(name, kind):
    m = re.search(r'pub struct ' + name + r"(?:<'info>)?\s*\{(.*?)\n\}", src, re.S)
    if not m: raise SystemExit('struct not found ' + name)
    body = m.group(1)
    fields, attrs = [], []
    for line in body.split('\n'):
        s = line.strip()
        if s.startswith('#['): attrs.append(s); continue
        if s.startswith('///') or s.startswith('//') or not s: continue
        fm = re.match(r'pub (\w+):\s*(.+?),?$', s)
        if not fm: continue
        fields.append((fm.group(1), fm.group(2).rstrip(','), ' '.join(attrs)))
        attrs = []
    return fields

ACC_STRUCTS = set(re.findall(r"#\[derive\(Accounts\)\]\s*pub struct (\w+)", src))

def accounts(name):
    res = []
    for fname, ftype, attr in struct_fields(name, 'acc'):
        base = re.match(r'(?:Box<)?(\w+)', ftype).group(1)
        if base in ACC_STRUCTS:
            res.append({'name': fname, 'accounts': accounts(base)}); continue
        a = {'name': fname}
        inner = re.search(r'#\[account\((.*)\)\]', attr)
        toks = inner.group(1) if inner else ''
        if re.search(r'(^|[\s,(])(mut|init|init_if_needed)([\s,)]|$)', toks): a['writable'] = True
        if base == 'Signer': a['signer'] = True
        res.append(a)
    return res

prog = re.search(r'pub mod creatorfun_buyback \{(.*?)\n\}\n', src, re.S).group(1)
ixs = []
for m in re.finditer(r'pub fn (\w+)\(ctx: Context<(\w+)>(.*?)\) -> Result<\(\)>', prog):
    name, ctxname, rest = m.groups()
    args = [{'name': a, 'type': rtype(t)} for a, t in re.findall(r',\s*(\w+):\s*(\w+)', rest)]
    ixs.append({'name': name, 'discriminator': disc('global:' + name), 'accounts': accounts(ctxname), 'args': args})

def plain_struct(name):
    return {'name': name, 'type': {'kind': 'struct', 'fields': [{'name': f, 'type': rtype(t)} for f, t, _ in struct_fields(name, 'plain')]}}

events = re.findall(r'#\[event\]\s*pub struct (\w+)', src)
errs = re.search(r'pub enum BuybackError \{(.*?)\n\}', src, re.S).group(1)
errors = [{'code': 6000 + i, 'name': n, 'msg': msg} for i, (msg, n) in enumerate(re.findall(r'#\[msg\("(.*?)"\)\]\s*(\w+),', errs))]
idl = {
    'address': re.search(r'declare_id!\("(\w+)"\)', src).group(1),
    'metadata': {'name': 'creatorfun_buyback', 'version': '0.1.0', 'spec': '0.1.0', 'description': 'CREATORFUN permanent buyback, burn & donation'},
    'instructions': ixs,
    'accounts': [{'name': 'Vault', 'discriminator': disc('account:Vault')}],
    'events': [{'name': e, 'discriminator': disc('event:' + e)} for e in events],
    'errors': errors,
    'types': [plain_struct(e) for e in events] + [plain_struct('Vault')],
}
json.dump(idl, open(out, 'w'), indent=2)
print('idl:', len(ixs), 'instructions,', len(errors), 'errors,', len(events), 'events')
