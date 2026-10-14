// Execute the production accounting helper and report renderer.
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import assert from 'node:assert/strict';
const source=readFileSync('static/dashboard.js','utf8');
const extract=name=>{
  const match=source.match(new RegExp(`function ${name}\\([\\s\\S]*?\\n}`));
  assert.ok(match,`missing ${name}`);return match[0];
};
let rendered='';
const context=vm.createContext({Number,String,ledgerUsd:String,fmtMoney:String,
  ledgerEsc:String,ledgerSetHtml:(_,html)=>{rendered=html;}});
vm.runInContext(`${extract('realizedNetPnl')}
${extract('sumRealizedNetPnl')}
${extract('renderLedgerFigures')}`,context);
for (const mode of ['live','paper']) {
  context.trade={mode,pnl:1,fee_paid:0.2,accounting_version:0};
  assert.equal(vm.runInContext('realizedNetPnl(trade)',context),0.8);
}
context.trade={mode:'live',pnl:0.8,fee_paid:0.2,accounting_version:1};
assert.equal(vm.runInContext('realizedNetPnl(trade)',context),0.8);
vm.runInContext('renderLedgerFigures({net_pnl:0.8,booked_pnl:0.8,entry_fees:0.2},{})',context);
assert.match(rendered,/Booked P&L/);assert.match(rendered,/Entry fees/);
assert.doesNotMatch(rendered,/Gross P&L|Fees paid/);
console.log('dashboard accounting version and honest fee labels passed');

context.trade={pnl:0.1,fee_paid:0.2,accounting_version:0};
assert.equal(vm.runInContext('realizedNetPnl(trade)',context),-0.1);
context.trades=[{pnl:0.1,fee_paid:0.2,accounting_version:0},{pnl:0.4,fee_paid:0.1,accounting_version:1}];
assert.ok(Math.abs(vm.runInContext('sumRealizedNetPnl(trades)',context)-0.3)<1e-12);
context.trade={pnl:null,fee_paid:0.2};
assert.equal(vm.runInContext('realizedNetPnl(trade)',context),null);
context.trades.push(context.trade);
assert.equal(vm.runInContext('sumRealizedNetPnl(trades)',context),null);

const elements=new Map();
const statsContext=vm.createContext({Number,window:{},document:{getElementById:id=>{
  if(!elements.has(id))elements.set(id,{textContent:'old',className:''});return elements.get(id);}},
  fmtPct:value=>value==null?'—':String(value),fmt$:value=>value==null?'—':String(value),
  fmtN:value=>value==null?'—':String(value),clsPnl:()=>''});
vm.runInContext(extract('updateStatsBar'),statsContext);
statsContext.updateStatsBar({});
for(const id of ['st-total','st-winrate','st-pnl','st-sharpe','st-maxdd','st-open'])assert.equal(elements.get(id).textContent,'—',id);

statsContext.updateStatsBar({daily_pnl_evidence:{evidence_complete:false,conservative_unallocated_loss:0.5}},null);
assert.equal(elements.get('st-daily').textContent,'—');
assert.match(elements.get('st-daily').title,/unknown.*legacy partial sales/);
assert.match(elements.get('st-daily').title,/entries are blocked/);
statsContext.updateStatsBar({daily_pnl_evidence:{evidence_complete:true,day_utc:'2026-10-11'}},0);
assert.equal(elements.get('st-daily').textContent,'0');
assert.match(elements.get('st-daily').title,/2026-10-11 UTC.*partial sales/);
const dailyTitle=elements.get('st-daily').title;
statsContext.updateStatsBar({total_closed:2});
assert.equal(elements.get('st-daily').title,dailyTitle);
assert.match(readFileSync('static/index.html','utf8'),/>Closed P&amp;L<.*id="st-pnl"/);
assert.match(readFileSync('static/index.html','utf8'),/>Realized today<.*id="st-daily"/);
console.log('daily realized evidence and closed-cohort labels passed');
