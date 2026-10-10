#!/usr/bin/env python3
"""檢查HTML，不啟動dgxtop。預設嘗試實際file://；失敗才明示set_content。"""
from pathlib import Path
import json, os, sys
from playwright.sync_api import sync_playwright
ROOT=Path(__file__).resolve().parents[1]
OUT=Path(os.environ.get('DGX_SPEC_SCREENSHOTS',str(ROOT.parent/'dgxtop-spec-screenshots')))

def main():
 OUT.mkdir(parents=True,exist_ok=True)
 checks=[];errors=[];mode='file://'
 with sync_playwright() as p:
  browser=p.chromium.launch(executable_path=os.environ.get('CHROMIUM','/usr/bin/chromium'),headless=True,args=['--no-sandbox'])
  context=browser.new_context(viewport={'width':1440,'height':1000},reduced_motion='reduce')
  page=context.new_page();page.on('pageerror',lambda e:errors.append(str(e)))
  try:page.goto((ROOT/'index.html').as_uri(),wait_until='domcontentloaded',timeout=5000)
  except Exception as exc:
   mode='set_content（file://受管理政策阻擋）'
   page.close();page=context.new_page();page.on('pageerror',lambda e:errors.append(str(e)))
   page.set_content((ROOT/'index.html').read_text(),wait_until='load')
  page.evaluate('try{localStorage.clear()}catch(e){}')
  if mode=='file://':page.reload()
  else:page.set_content((ROOT/'index.html').read_text(),wait_until='load')
  assert page.locator('html').get_attribute('lang')=='zh-Hant-TW';checks.append('繁體中文lang與實際載入')
  assert page.locator('.boundary').count()==6 and page.locator('.req-table tbody tr').count()==40;checks.append('6邊界／40需求表')
  assert page.evaluate('document.documentElement.scrollWidth<=innerWidth');checks.append('桌面無頁面橫向溢出')
  page.screenshot(path=str(OUT/'desktop.png'),full_page=False)
  page.locator('#B1 [data-mark=agree]').click();assert page.locator('#B1 .status').inner_text()=='同意';checks.append('標記同意')
  page.locator('#note-B1').fill('預算與原始證據都要保留');page.locator('#status-filter').select_option('question');assert page.locator('.boundary:visible').count()==0;checks.append('狀態篩選')
  page.locator('#status-filter').select_option('all');page.locator('#B2 [data-mark=question]').click();page.locator('#status-filter').select_option('question');assert page.locator('.boundary:visible').count()==1;checks.append('只顯示有疑問')
  page.locator('#status-filter').select_option('all');page.locator('#note-B3').focus();page.keyboard.type('123jk');assert page.locator('#B3 .status').inner_text()!='不同意';checks.append('文字輸入停用快捷鍵')
  page.locator('h1').first.click();page.keyboard.press('j');page.keyboard.press('3');assert page.locator('.boundary.current .status').inner_text()=='不同意';checks.append('j/k及1/2/3標記快捷鍵')
  page.locator('#overall-note').fill('同意架構；效能要附真實資料。')
  page.locator('#export-feedback').click();txt=page.locator('#copy-text').input_value();assert txt.startswith('審查 ID：dgxtop-spec | 版本：2.0.0') and 'B1 Adapter' in txt and txt.endswith('列出處理結果。');checks.append('回饋含ID／版本／標記／備註／未審閱數')
  page.locator('#copy-dialog [data-close]').click()
  # Force clipboard failure to test fallback, even if native clipboard might work.
  page.evaluate("Object.defineProperty(navigator,'clipboard',{configurable:true,value:{writeText:()=>Promise.reject(new Error('blocked by test'))}})")
  page.locator('#copy-prompt').click();page.wait_for_timeout(100);assert '剪貼簿不可用' in page.locator('#copy-status').inner_text();assert page.evaluate("document.querySelector('#copy-text').selectionEnd===document.querySelector('#copy-text').value.length");checks.append('剪貼簿失敗：選取完整文字fallback')
  assert '全部Rust實作與測試' in page.locator('#copy-text').input_value();checks.append('實作指令可複製且完整')
  page.locator('#copy-dialog [data-close]').click()
  for i in range(1,5):
   page.locator(f'[data-zoom=V{i}]').click();assert page.locator('#zoom-dialog').is_visible();assert page.locator('#zoom-content svg').count()==1
   color=page.locator('#zoom-content svg text').first.evaluate('(e)=>getComputedStyle(e).fill');assert color not in ('rgb(0, 0, 0)','black'),color
   ids=page.locator('[id]').evaluate_all('(els)=>els.map(x=>x.id)');assert len(ids)==len(set(ids))
   page.locator('#zoom-dialog [data-close]').click()
  checks.append('4幅SVG可見／放大／文字樣式／ID唯一')
  page.locator('#sidebar a[href="#doc-performance-md"]').click();assert page.locator('#doc-performance-md').get_attribute('open') is not None;checks.append('目錄自動展開指定權威文件')
  page.locator('.req-table a[href="#at32"]').click();assert page.locator('#doc-acceptance-md').get_attribute('open') is not None;checks.append('需求表跳轉精確驗收ID')
  if mode=='file://':
   page.reload();assert page.locator('#note-B1').input_value()=='預算與原始證據都要保留';checks.append('file:// localStorage重新載入保留回饋')
  page.locator('#collapse-docs').click();assert page.locator('.doc[open]').count()==0;checks.append('文件全部收合')
  page.locator('#expand-docs').click();assert page.locator('.doc[open]').count()==page.locator('.doc').count();checks.append('文件全部展開')
  page.locator('#collapse-docs').click();page.locator('a[href="#overview"]').first.click();page.set_viewport_size({'width':390,'height':844})
  assert page.evaluate('document.documentElement.scrollWidth<=innerWidth');checks.append('390px手機無頁面橫向溢出')
  page.screenshot(path=str(OUT/'mobile.png'),full_page=False)
  page.locator('#menu-button').click();assert page.locator('#sidebar').evaluate("e=>e.classList.contains('open')");page.locator('#sidebar a[href="#boundaries"]').click();assert page.locator('#menu-button').get_attribute('aria-expanded')=='false';checks.append('手機目錄開關與跳轉')
  # localStorage unavailable: separate DOM load with blocked storage, no false persistence.
  blocked=context.new_page();blocked.on('pageerror',lambda e:errors.append(str(e)));blocked.add_init_script("Object.defineProperty(window,'localStorage',{get(){throw new Error('blocked by test')}})")
  if mode=='file://':blocked.goto((ROOT/'index.html').as_uri(),wait_until='load')
  else:
   blocked.evaluate("Object.defineProperty(window,'localStorage',{get(){throw new Error('blocked by test')}})");blocked.set_content((ROOT/'index.html').read_text())
  blocked.locator('#B1 [data-mark=agree]').click();assert blocked.locator('#B1 .status').inner_text()=='同意';assert '記憶體' in blocked.locator('#storage-state').inner_text();checks.append('localStorage禁止仍可記憶體標記')
  assert not errors,errors;checks.append('無JavaScript pageerror')
  browser.close()
 result={'status':'PASS','scope':'HTML瀏覽器功能，非Rust或GPU驗收','navigation':mode,'engine':'Chromium','checks':checks,'page_errors':errors,'clipboard':'強制拒絕分支已測；OS剪貼簿貼入外部應用未驗證','screenshots':'僅供本次本地視覺檢查，PNG不納入規格ZIP'}
 (ROOT/'validation/browser-results.json').write_text(json.dumps(result,ensure_ascii=False,indent=2)+'\n')
 print(json.dumps(result,ensure_ascii=False,indent=2))
if __name__=='__main__':main()
