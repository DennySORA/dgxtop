"""配額／時間與動作競態的合成模型；不是Rust程式、硬體測試或效能量測。"""
from dataclasses import dataclass,field
import math

@dataclass
class Ledger:
    capacity:int
    items:int
    used:int=0
    allocations:dict=field(default_factory=dict)
    def acquire(self,key,size):
        if size<0 or key in self.allocations:raise ValueError('invalid permit')
        if len(self.allocations)>=self.items or self.used+size>self.capacity:return False
        self.allocations[key]=size;self.used+=size;return True
    def release(self,key):
        if key not in self.allocations:raise ValueError('double release')
        self.used-=self.allocations.pop(key)

def next_tick(base,period,now):
    if period<=0 or now<base:raise ValueError('invalid clock/period')
    return base+((now-base)//period+1)*period

def counter_interval(prev,value,start,end,window_start,window_end):
    if value<prev or end<=start:return (None,0,'reset')
    if start<window_start or end>window_end:return (None,0,'boundary_gap')
    return (value-prev,end-start,'counter_delta')

def weighted_gauge(segments,start,end):
    if end<=start:raise ValueError('invalid window')
    total=0.;covered=0;last_end=start
    for a,b,value in sorted(segments):
        if a<last_end and b>start:raise ValueError('overlap must be deduplicated before aggregation')
        x,y=max(a,start),min(b,end)
        if y>x and value is not None and math.isfinite(value):
            total+=value*(y-x);covered+=y-x
        last_end=max(last_end,b)
    return (total/covered if covered else None,covered/(end-start))

@dataclass
class Action:
    expires:int
    revision:int
    state:str='prepared'
    send_count:int=0
    def confirm(self,now,revision):
        if self.state!='prepared' or now>=self.expires or revision!=self.revision:return False
        self.state='intent_pending';return True
    def cancel(self):
        if self.state in ('prepared','intent_pending','intent_committed'):
            self.state='cancelled';return True
        return False
    def durable_ack(self,now,revision):
        if self.state!='intent_pending' or now>=self.expires or revision!=self.revision:return False
        self.state='sending';self.send_count+=1;return True
    def crash(self):
        if self.state in ('sending','intent_pending','intent_committed'):self.state='unknown'

@dataclass
class Pressure:
    state:str='normal'
    consecutive:int=0
    healthy_since:int|None=None
    pressure_since:int|None=None
    def update(self,queue,memory,now):
        if memory>=.90:
            self.state='throttled';self.healthy_since=None;return self.state
        if queue>=.75:
            self.consecutive+=1;self.healthy_since=None
            if self.pressure_since is None:self.pressure_since=now
            if self.consecutive>=3 and now-self.pressure_since>=3000 and self.state=='normal':self.state='pressured'
        elif queue<.5 and memory<.75:
            self.consecutive=0;self.pressure_since=None
            if self.healthy_since is None:self.healthy_since=now
            if self.state!='normal' and now-self.healthy_since>=30000:
                self.state='recovering' if self.state=='throttled' else 'normal'
                self.healthy_since=now
        else:self.healthy_since=None;self.consecutive=0;self.pressure_since=None
        return self.state
