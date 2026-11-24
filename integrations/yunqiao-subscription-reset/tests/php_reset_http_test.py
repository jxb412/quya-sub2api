import base64,hashlib,hmac,json,os,pathlib,socket,subprocess,sys,time,urllib.error,urllib.parse,urllib.request

root=pathlib.Path(__file__).resolve().parent.parent
php=sys.argv[1] if len(sys.argv)>1 else r'D:\btc\st\.tools\php-7.4.33\php.exe'
candidate=pathlib.Path(sys.argv[2]) if len(sys.argv)>2 else root/'php-candidate'
with socket.socket() as s:s.bind(('127.0.0.1',0));port=s.getsockname()[1]
env=os.environ.copy();env['QUYA_RESET_TEST_ROOT']=str(candidate)
server=subprocess.Popen([php,'-S',f'127.0.0.1:{port}',str(root/'tests/php_reset_http_router.php')],env=env,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
base=f'http://127.0.0.1:{port}'
body=base64.urlsafe_b64encode(json.dumps({'user_id':42,'expires_at':int(time.time())+3600},separators=(',',':')).encode()).decode().rstrip('=')
cookie='sub2apipay_session='+body+'.'+hmac.new(b'test-http-secret',body.encode(),hashlib.sha256).hexdigest()
passed=[]
class NoRedirect(urllib.request.HTTPRedirectHandler):
 def redirect_request(self,*args,**kwargs):return None
opener=urllib.request.build_opener(NoRedirect())
def request(path,method='GET',data=None,auth=True,csrf=None):
 headers={}
 if auth:headers['Cookie']=cookie
 if csrf:headers['X-Subscription-Reset-CSRF']=csrf
 if data is not None:headers['Content-Type']='application/json';data=json.dumps(data).encode()
 req=urllib.request.Request(base+path,data=data,method=method,headers=headers)
 try:r=opener.open(req,timeout=5)
 except urllib.error.HTTPError as e:r=e
 return r.status,dict(r.headers),r.read().decode('utf-8')
def check(name,condition):
 if not condition:raise AssertionError(name)
 passed.append(name)
try:
 for _ in range(40):
  try:request('/ready');break
  except OSError:time.sleep(.05)
 status,headers,text=request('/api/user/subscription-reset/preview?subscription_id=100',auth=False)
 check('unauthenticated preview is 401 JSON',status==401 and json.loads(text)['code']==401)
 status,headers,text=request('/api/user/subscription-reset/preview?subscription_id=100&user_id=999')
 check('preview identity comes from signed cookie',status==200 and json.loads(text)['data']['actor_id']==42)
 check('personal endpoint is not cacheable','no-store' in headers.get('Cache-Control',''))
 status,_,text=request('/api/user/subscription-reset/preview?subscription_id=abc')
 check('invalid subscription ID rejected',status==400)
 status,_,text=request('/api/user/subscription-reset/execute','POST',{'accepted_rules':True,'quote_token':'safe'},csrf=None)
 check('POST requires CSRF header',status==400)
 status,_,text=request('/api/user/subscription-reset/execute','POST',{'accepted_rules':False,'quote_token':'safe'},csrf='csrf-42')
 check('explicit confirmation required',status==400)
 status,_,text=request('/api/user/subscription-reset/execute','POST',{'accepted_rules':True,'quote_token':'safe','user_id':999},csrf='csrf-42')
 check('POST ignores forged browser user ID',status==200 and json.loads(text)['data']['actor_id']==42)
 status,_,text=request('/api/user/subscription-reset/execute','POST',{'accepted_rules':True,'quote_token':'pending'},csrf='csrf-42')
 check('unconfirmed result gives retryable 503',status==503 and json.loads(text)['data']['retryable'])
 status,_,text=request('/api/user/subscription-reset/execute','POST',{'accepted_rules':True,'quote_token':'busy'},csrf='csrf-42')
 check('local lock conflict gives retryable 409',status==409 and json.loads(text)['data']['retryable'])
 status,_,text=request('/api/user/subscription-reset/execute')
 check('GET cannot execute reset',status==405)
 status,_,text=request('/pay/api/user/subscription-reset/preview?subscription_id=100')
 check('pay mount API supported',status==200)
 query=urllib.parse.urlencode({'token':'test-login-token','user_id':42,'subscription_id':100,'src_host':'https://member.example.invalid','theme':'dark'})
 status,headers,text=request('/subscription-reset?'+query,auth=False)
 location=headers.get('Location','')
 check('embedded login redirects after cookie exchange',status==302 and 'sub2apipay_session=' in headers.get('Set-Cookie',''))
 check('redirect preserves chosen subscription and parent origin','subscription_id=100' in location and 'src_host=' in location and 'token=' not in location)
 status,headers,text=request('/subscription-reset?subscription_id=100&theme=dark')
 check('confirmation page renders',status==200 and '确认重置并启用下一期' in text and 'quya:subscription-reset-completed' in text)
 print(json.dumps({'passed':len(passed),'tests':passed},ensure_ascii=False,indent=2))
finally:
 server.terminate()
 try:server.wait(timeout=5)
 except subprocess.TimeoutExpired:server.kill();server.wait()
