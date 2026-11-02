"""Normal feature self-test in a new owned PG. No old review/probe execution.
Credentials and native one-time codes remain private, outside evidence.
"""
from pathlib import Path
import os,json,secrets,subprocess,socket,re,time,http.client,uuid,threading,queue
from urllib.parse import quote
import argparse
ap=argparse.ArgumentParser();ap.add_argument('--runtime',type=Path,required=True);ap.add_argument('--output',type=Path,required=True);ap.add_argument('--binaries',type=Path,required=True);ap.add_argument('--pg-bin',type=Path,required=True);ap.add_argument('--name',required=True);a=ap.parse_args()
S=Path(__file__).resolve().parents[1];R=a.runtime.resolve();E=a.output.resolve();BIN=a.binaries.resolve();PG=a.pg_bin.resolve();B=S/'artifacts/foundation-draft-lifecycle-2026-10-04'
assert R.is_absolute() and E.is_absolute() and not R.is_relative_to(S) and not E.is_relative_to(S)
assert re.fullmatch(r'[a-z0-9-]{1,48}',a.name)
R.mkdir(parents=True,exist_ok=True);E.mkdir(parents=True,exist_ok=True);(R/'tmp').mkdir(exist_ok=True)
name=a.name;G=R/('step12-'+name);O=E/name;G.mkdir();O.mkdir();data=G/'pgdata'
(O/'RUNNER.py').write_bytes(Path(__file__).read_bytes())
env={k:v for k,v in os.environ.items() if not re.match(r'(?i)^(SAFEFLOW_|SF_|PG|DATABASE_URL$|BILLING_|AUDIT_|.*API_KEY$)',k)}
env.update(TMP=str(R/'tmp'),TEMP=str(R/'tmp'))
for k,v in json.loads((S/'package-tools/fixture-closure.json').read_text('utf8')).get('fixture_env',{}).items():env[k]=str(S/v)
admin='sf_bank_admin';role='sf_bank_api';pw={admin:secrets.token_hex(32),role:secrets.token_hex(32)};secret=secrets.token_hex(32);control=secrets.token_hex(32)
db='billing_test_bank_site';ns='sf_unified_bank_site';cookie=None;started=False;hosts=[];transcript=[];cfg=None

def save(n,v):
 with (O/n).open('x',encoding='utf8') as f:json.dump(v,f,ensure_ascii=False,indent=2);f.write('\n')
def port():
 with socket.socket() as p:p.bind(('127.0.0.1',0));return p.getsockname()[1]
pgport=port();hport=port();fport=port()
def url(who=admin):return f'postgresql://{who}:{quote(pw[who])}@127.0.0.1:{pgport}/{db}?sslmode=disable'
def sql(q,who=admin):
 p=subprocess.run([str(PG/'psql.exe'),'-X','-qAt','-h','127.0.0.1','-p',str(pgport),'-U',who,'-d',db,'-v','ON_ERROR_STOP=1'],input=f'SET search_path TO "{ns}";'+q,env={**env,'PGPASSWORD':pw[who]},capture_output=True,text=True,encoding='utf8',timeout=30,creationflags=subprocess.CREATE_NO_WINDOW)
 assert p.returncode==0,p.stderr;return p.stdout.strip()
def pg(n,args):
 with (O/(n+'.log')).open('xb') as fd:p=subprocess.run([str(PG/(args[0]+'.exe')),*args[1:]],env={**env,'PGPASSWORD':pw[admin]},stdout=fd,stderr=subprocess.STDOUT,timeout=60,creationflags=subprocess.CREATE_NO_WINDOW)
 save(n+'.exit.json',{'args':args,'exit':p.returncode});assert p.returncode==0,n

def scrub(v):
 if isinstance(v,dict):return {k:('OMITTED' if k in ['login_secret','controller_secret','code','cookie','database_url'] else scrub(i)) for k,i in v.items()}
 if isinstance(v,list):return [scrub(i) for i in v]
 return v

def request(p,body=None,method=None,frontend=False):
 c=http.client.HTTPConnection('127.0.0.1',fport if frontend else hport,timeout=40)
 try:
  headers={'Content-Type':'application/json','Origin':f'http://127.0.0.1:{fport}','x-safeflow-request':'1'}
  if cookie:headers['Cookie']=cookie
  c.request(method or ('GET' if body is None else 'POST'),p,None if body is None else json.dumps(body),headers);r=c.getresponse();raw=r.read();v=json.loads(raw) if raw else None;transcript.append({'path':p,'method':method,'body':scrub(body),'status':r.status,'response':scrub(v),'via_frontend':frontend});return r.status,v,r.getheader('set-cookie')
 finally:c.close()
def ok(p,body=None):
 status,v,_=request(p,body);assert status in [200,201],(p,status,v);return v

def native(command,extra=None):
 command=dict(command)
 if command.get('kind') in ['CHALLENGE','CARD_CHALLENGE']:command['controller_secret']=control;command['cookie']=cookie
 h=hosts[-1];h['child'].stdin.write(json.dumps(command)+'\n');h['child'].stdin.flush();v=h['queue'].get(timeout=20);assert isinstance(v,dict),v;return v

def config():
 p=G/'bank.json';v={'schema':ns,'owner':'user_001','bind':f'127.0.0.1:{hport}','frontend_origin':f'http://127.0.0.1:{fport}','recovery_realm':str(uuid.uuid4()),'confirmation_seconds':300,'permit_seconds':60};p.write_text(json.dumps(v));return p,v

def cli(command):
 creds={'database_url':url(),'login_secret':secret,'controller_secret':control,'api_role':role,'api_password':pw[role]}
 p=subprocess.run([str(BIN/'bank_site.exe'),str(cfg),command],input=json.dumps(creds)+'\n',cwd=B,env=env,capture_output=True,text=True,encoding='utf8',timeout=80,creationflags=subprocess.CREATE_NO_WINDOW)
 save('provision.json',{'exit':p.returncode,'stdout':p.stdout,'stderr':p.stderr});assert p.returncode==0,p.stderr

def serve(label):
 c=subprocess.Popen([str(BIN/'bank_site.exe'),str(cfg),'serve'],cwd=B,env=env,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=(O/(label+'.stderr')).open('xb'),text=True,encoding='utf8',bufsize=1,creationflags=subprocess.CREATE_NO_WINDOW)
 q=queue.Queue()
 def read():
  for line in c.stdout:
   try:q.put(json.loads(line))
   except ValueError:q.put({'unparsed':line})
 threading.Thread(target=read,daemon=True).start();h={'child':c,'queue':q};hosts.append(h)
 c.stdin.write(json.dumps({'database_url':url(role),'login_secret':secret,'controller_secret':control,'api_role':None,'api_password':None})+'\n');c.stdin.flush()
 v=q.get(timeout=35);assert v.get('ready'),v;save(label+'.ready.json',v)
 return h

def stop(h):
 if h['child'].poll() is None:h['child'].stdin.write('{"kind":"STOP"}\n');h['child'].stdin.flush();h['child'].wait(timeout=25)
 assert h['child'].returncode==0

def login():
 global cookie
 status,v,c=request('/api/sim/session',{'login_secret':secret});assert status==200,v;cookie=c.split(';')[0]

def change(v):return ok('/api/subscriptions/change',v)
def transact(r):
 p=change({'kind':'PREPARE','nonce':str(uuid.uuid4()),'request':r});change({'kind':'CONFIRM','task_id':p['task_id'],'confirmation_ref':p['confirmation_ref'],'explicitly_confirm':True});v=change({'kind':'EXECUTE','task_id':p['task_id'],'operation_id':p['operation_id'],'confirmation_ref':p['confirmation_ref']});assert v['domain_audit']['status']=='VERIFIED_CANDIDATE_V1';return p,v

def select(name):return next(i['selection_ref'] for i in ok('/api/subscriptions',{'kind':'LIST'})['items'] if name in str(i))

try:
 pwfile=G/'init.pw';pwfile.write_text(pw[admin]+'\n')
 try:pg('init',['initdb','-D',str(data),'-U',admin,'--auth=scram-sha-256','--pwfile='+str(pwfile),'--encoding=UTF8','--locale=C'])
 finally:pwfile.unlink(missing_ok=True)
 with (data/'postgresql.conf').open('a') as f:f.write("\nlog_statement='none'\nlog_min_error_statement='panic'\npassword_encryption='scram-sha-256'\n")
 pg('start',['pg_ctl','-D',str(data),'-l',str(G/'postgres.log'),'-w','-t','30','-o',f'-h 127.0.0.1 -p {pgport}','start']);started=True
 pg('createdb',['createdb','-h','127.0.0.1','-p',str(pgport),'-U',admin,db]);cfg,configvalue=config();save('OWNER.json',{'data':str(data),'pgport':pgport,'apiport':hport,'frontendport':fport,'schema':ns,'db':db})
 cli('provision');h=serve('host');login()
 print('owned bank host ready',flush=True)
 before=ok('/api/bank/overview');save('BALANCES-BEFORE.json',before)
 v=ok('/api/team-demo/billing',{});save('MONTHLY.json',v);r=v['results'][0]['data'];m=r['monthly_report'];assert m['comparison']['current']['net']=='-157.20',m;assert m['comparison']['percent']=='-141.4';assert r['report']['metadata']['report_id'].endswith(':current')
 assert sql('SELECT count(*) FROM public_billing_reports_v1')=='1'
 context=ok('/api/billing-feedback/v2/context');nonce=str(uuid.uuid4());realm=context['recovery_realm'];p=ok('/api/billing-feedback/v2/preparations',{'contract_version':2,'recovery_realm':realm,'nonce':nonce,'intent':{'input_version':1,'report_id':r['report']['metadata']['report_id'],'transaction':r['report']['evidence'][0]['reference'],'choice':'NOT_MINE'}})
 action={k:p[k] for k in ['task_id','operation_id']};action.update(contract_version=2,recovery_realm=realm,confirmation_ref=p['confirmation']['confirmation_ref'])
 ok(f'/api/billing-feedback/v2/preparations/{nonce}/confirm',action);feedback=ok(f'/api/billing-feedback/v2/preparations/{nonce}/execute',action);assert feedback['state']=='SAVED';save('FEEDBACK.json',feedback)
 cap=change({'kind':'CAPTURE'});groups=cap['preview']['recognition']['groups'];index=next(g['selection_index'] for g in groups if g['charges'] and all(t['amount_minor_units']==1990 for t in g['charges']))
 subp,_=transact({'kind':'REGISTRATION','capture_ref':cap['capture_ref'],'candidate_index':index});transact({'kind':'REMINDER_SETTINGS','selection_ref':select('演示视频服务'),'enabled':True,'days_before':1});transact({'kind':'RECORD_CANCELLATION','selection_ref':select('演示视频服务')});transact({'kind':'REVOKE_CANCELLATION','selection_ref':select('演示视频服务')});assert ok('/api/subscriptions',{'kind':'REMINDER_ATTEMPTS'})['items']==[]
 # Ordinary user flow: original nonce before draft -> explicit confirmations.
 n=str(uuid.uuid4());d=ok('/api/bank/payment-draft',{'nonce':n});assert d['parameters']['amount']=='100.00';task=d['task_id'];op=d['operation_id'];lp=f'/api/sim/draft-lifecycles/{task}/operations/{op}';scope={'contract_version':2,'scope':'MOCK_DRAFT_LIFECYCLE_ONLY'}
 d=ok(lp+'/prepare',scope);d=ok(lp+'/confirm',{**scope,'confirmation_ref':d['confirmation']['confirmation_ref'],'display_ticket':d['display_ticket'],'action':'CONFIRM'});d=ok(lp+'/execute',{**scope,'confirmation_ref':d['confirmation']['confirmation_ref'],'task_revision':d['task_revision'],'operation_revision':d['operation_revision'],'plan_version':d['plan_version'],'facts_version':d['facts_version']});assert d['execution_status']=='SUCCEEDED'
 p=ok('/api/sim/payment-preparations/'+task,{'contract_version':1,'scope':'PAYMENT_INTENT_ONLY','source':{'selection_ticket':d['selection_ticket'],'task_revision':d['task_revision'],'plan_version':d['plan_version'],'facts_version':d['facts_version'],'draft_id':d['receipt']['draft_id'],'draft_version':d['receipt']['draft_version']}})
 pbase='/api/sim/payment-preparations/'+task+'/'+p['operation']['operation_id'];p=ok(pbase+'/confirm',{'contract_version':1,'scope':'PAYMENT_INTENT_ONLY','confirmation_ref':p['confirmation_ref'],'display_ticket':p['display_ticket'],'action':'CONFIRM'})
 def background(action):return ok(pbase+'/background',{'contract_version':1,'scope':'EXPLICIT_PAYMENT_BACKGROUND_V1','action':action})
 shown=background({'kind':'DISPLAY'});background({'kind':'CONFIRM','shown':shown});challenge=native({'kind':'CHALLENGE','task_id':task,'operation_id':p['operation']['operation_id'],'consent_id':shown['consent_id']});assert background({'kind':'VERIFY_CODE','consent_id':shown['consent_id'],'code':challenge['code']})['verified'];background({'kind':'EXECUTE','consent_id':shown['consent_id'],'confirmation_ref':p['confirmation_ref'],'display_ticket':p['display_ticket']})
 pending=ok(pbase+'/funds-result');save('PAYMENT-PENDING.json',pending)
 # Independent real producer and worker, never a browser decision.
 gid=sql("SELECT grant_id FROM payment_completion_candidate_grants WHERE preparation_operation_id='"+p['operation']['operation_id']+"'")
 def actor(kind,command,register=False):
  actorrole='sf_bank_'+kind.lower()
  if actorrole not in pw:pw[actorrole]=secrets.token_hex(32)
  configp=G/('actor-'+kind+'.json');configp.write_text(json.dumps({'namespace':ns,'kind':kind,'role_name':actorrole}))
  mode='register' if register else 'run'
  creds={'database_url':url() if register else url(actorrole),'new_role_password':pw[actorrole] if register else None}
  commands=[] if register else [command,{'kind':'STOP'}]
  p1=subprocess.run([str(BIN/'payment_background_actor.exe'),str(configp),mode],input='\n'.join(json.dumps(i) for i in [creds,*commands])+'\n',cwd=B,env=env,capture_output=True,text=True,encoding='utf8',timeout=60,creationflags=subprocess.CREATE_NO_WINDOW)
  assert p1.returncode==0,p1.stderr
  result=[json.loads(line) for line in p1.stdout.splitlines()]
  assert all('error' not in v for v in result),result
  return result[-1]
 actor('PRODUCER',None,True);actor('WORKER',None,True)
 configp=G/'actor-WORKER.json';p1=subprocess.run([str(BIN/'payment_background_actor.exe'),str(configp),'enable-final'],input=json.dumps({'database_url':url(),'new_role_password':None})+'\n',cwd=B,env=env,capture_output=True,text=True,encoding='utf8',timeout=30,creationflags=subprocess.CREATE_NO_WINDOW);assert p1.returncode==0,p1.stderr
 actor('PRODUCER',{'kind':'SETTLE','grant_id':gid,'result_id':'bank-normal-original-result'})
 completed=actor('WORKER',{'kind':'COMPLETE','grant_id':gid});save('WORKER-COMPLETION.json',completed);assert completed['state']=='COMPLETED'
 funds=ok(pbase+'/funds-result');assert funds['state']=='SUCCEEDED',funds;save('PAYMENT-COMPLETED.json',funds)
 assert sql('SELECT count(*) FROM transfer_ledger_v1')=='2'
 again=actor('WORKER',{'kind':'COMPLETE','grant_id':gid});assert completed==again;assert sql('SELECT count(*) FROM transfer_ledger_v1')=='2'
 after=ok('/api/bank/overview');assert after['accounts'][0]['balance']['available_balance']=='1188.00';save('BALANCES-AFTER.json',after)
 cp=ok('/api/card-safety',{'kind':'PREPARE','account_id':'account_001','card_id':'demo_card_salary_v1','nonce':str(uuid.uuid4())});cop=cp['operation_id'];ok('/api/card-safety',{'kind':'CONFIRM','operation_id':cop,'confirmation_ref':cp['confirmation_ref']});code=native({'kind':'CARD_CHALLENGE','operation_id':cop});assert ok('/api/card-safety',{'kind':'VERIFY_CODE','operation_id':cop,'challenge_id':code['challenge_id'],'code':code['code']})['verified'];ok('/api/card-safety',{'kind':'ISSUE_PERMIT','operation_id':cop});freeze=ok('/api/card-safety',{'kind':'EXECUTE_FREEZE','operation_id':cop});assert freeze['business_executed'] is True,freeze;save('SELF-FREEZE.json',freeze)
 original=ok('/api/card-safety',{'kind':'QUERY_ORIGINAL','operation_id':cop});assert original['original_lock_active'] is True
 balance_frozen=ok('/api/bank/overview');assert balance_frozen['accounts'][0]['balance']['available_balance']=='1188.00'
 # Same original queries after logout, host and actual PostgreSQL restart.
 reportid=v['task']['task_id'];oldcookie=cookie;request('/api/sim/session',{},method='DELETE');assert request('/api/bank/overview')[0]==401
 stop(h);pg('restart-stop',['pg_ctl','-D',str(data),'-w','-t','30','-m','fast','stop']);pg('restart-start',['pg_ctl','-D',str(data),'-l',str(G/'postgres.log'),'-w','-t','30','-o',f'-h 127.0.0.1 -p {pgport}','start']);h=serve('host-restart');cookie=oldcookie;assert request('/api/bank/overview')[0]==401;login()
 assert ok(pbase+'/funds-result')==funds;assert ok('/api/card-safety',{'kind':'QUERY_ORIGINAL','operation_id':cop})==original
 assert change({'kind':'QUERY','operation_id':subp['operation_id']})['domain_audit']['status']=='VERIFIED_CANDIDATE_V1'
 recovered=ok('/api/agent/tasks/'+reportid);assert recovered['results'][0]['data']==r
 assert ok('/api/billing-feedback/v2/preparations/'+nonce+'?recovery_realm='+realm)['state']=='SAVED'
 assert sql('SELECT count(*) FROM transfer_ledger_v1')=='2'
 save('RESTART.json',{'pg_and_host':True,'old_cookie_rejected':True,'payment_equal':True,'card_equal':True,'original_report_equal':True,'feedback_saved':True,'ledger_rows':2})
 stop(h)
 # Other two adopted users use their actual independent captures, not relabelled reports.
 for who,expected in [('user_002','25.00'),('demo_user_li_v1','0.00')]:
  configvalue['owner']=who;cfg.write_text(json.dumps(configvalue));cookie=None;h=serve('host-'+who);login();other=ok('/api/team-demo/billing',{})['results'][0]['data']['monthly_report'];assert other['comparison']['current']['net']==expected;assert other['comparison']['percent'] is None;save('MONTHLY-'+who+'.json',other);stop(h)
 save('FEATURE-RESULT.json',{'passed':True,'monthly_three_owners':True,'feedback_current_original':True,'subscription_four_actions':True,'payment_100_success':True,'worker_independent':True,'ledger_exactly_two':True,'balance_1288_to_1188':True,'self_freeze':True,'originals_pg_host_restart':True,'old_cookie_rejected':True,'full_concurrency_unknown_commit_matrix':'NOT_RUN'})

except BaseException as e:
 save('FAILURE.json',{'type':type(e).__name__,'message':str(e)});raise
finally:
 for h in hosts:
  if h['child'].poll() is None:stop(h)
 if started and (data/'postmaster.pid').exists():
  lines=(data/'postmaster.pid').read_text().splitlines();assert Path(lines[1]).resolve()==data.resolve() and int(lines[3])==pgport;pg('stop',['pg_ctl','-D',str(data),'-w','-t','30','-m','fast','stop'])
 save('HTTP.json',transcript);save('CLEANUP.json',{'pg_running':(data/'postmaster.pid').exists(),'hosts':[{'pid':h['child'].pid,'exit':h['child'].poll()} for h in hosts]})
