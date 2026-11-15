import pathlib,tempfile,subprocess,os,hashlib,fcntl
script=pathlib.Path(__file__).resolve().parents[1]/'promote_local_package.sh'
with tempfile.TemporaryDirectory(prefix='ticket93-promotion-test-') as tmp:
 root=pathlib.Path(tmp); package=root/'candidate'; (package/'bin').mkdir(parents=True)
 (package/'codex-package.json').write_text('{"layoutVersion":1,"version":"0.0.0","target":"x86_64-unknown-linux-gnu","variant":"codex","entrypoint":"bin/codex"}')
 for name in ['codex','codex-code-mode-host']:
  f=package/'bin'/name; f.write_text('#!/bin/bash\necho "codex-cli 0.0.0"\n');f.chmod(0o755)
 (package/'codex').symlink_to('bin/codex')
 checks=package/'SHA256SUMS'; checks.write_text(''.join(hashlib.sha256(f.read_bytes()).hexdigest()+'  '+str(f.relative_to(package))+'\n' for f in package.rglob('*') if f.is_file() and not f.is_symlink()))
 shim=root/'shim';shim.mkdir(); fake=shim/'systemd-run'; fake.write_text('#!/bin/bash\n[[ ! -e /proc/$$/fd/9 ]] || exit 91\nprintf "%s\\n" "$*" > "$TEST_PROMOTION_RECEIPT"\n');fake.chmod(0o755)
 home=root/'home';bindir=root/'bin';standalone=home/'packages/standalone';standalone.mkdir(parents=True)
 env=dict(os.environ,PATH=str(shim)+':'+os.environ['PATH'],TEST_PROMOTION_RECEIPT=str(root/'receipt'))
 cmd=['bash',str(script),'--package-dir',str(package),'--release-name','local-test-x86_64-unknown-linux-gnu','--checksums',str(checks),'--codex-home',str(home),'--bin-dir',str(bindir)]
 def run(args): return subprocess.run(args,env=env,capture_output=True,text=True,timeout=10)
 result=run(cmd);assert result.returncode==0,result.stderr
 assert not (standalone/'current').exists(); print('PASS: validation makes no selection or daemon update')
 with (standalone/'install.lock').open('w') as lock:
  fcntl.flock(lock,fcntl.LOCK_EX)
  result=run(cmd+['--promote']);assert result.returncode!=0 and 'install lock is held' in result.stderr,result.stderr
 assert not (standalone/'current').exists(); print('PASS: busy lock fails promptly without selecting package')
 result=run(cmd+['--promote']);assert result.returncode==0,result.stderr
 assert (root/'receipt').is_file();assert (standalone/'current/codex').resolve()==(standalone/'current/bin/codex').resolve()
 print('PASS: existing candidate entrypoint symlink stages successfully')
 print('PASS: detached update launcher does not inherit install lock')
 with (standalone/'install.lock').open('w') as lock: fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)
 print('PASS: installation lock released')
