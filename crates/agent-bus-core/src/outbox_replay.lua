local function same(left,right)
  if type(left)~=type(right) then return false end
  if type(left)~='table' then return left==right end
  for key,value in pairs(left) do if not same(value,right[key]) then return false end end
  for key,_ in pairs(right) do if left[key]==nil then return false end end
  return true
end
-- Dedicated authenticated replay. Rust validates schema and derives every key.
-- Scripts are isolated, not rollback transactions: reserve first, and a runtime
-- failure leaves PENDING instead of blindly executing partially applied writes.
local input = cjson.decode(ARGV[1])
local time = redis.call('TIME')
local now = tonumber(time[1]) * 1000 + math.floor(tonumber(time[2]) / 1000)
local prior = redis.call('GET', KEYS[1])
if prior then
  local record = cjson.decode(prior)
  if record.fingerprint ~= input.fingerprint then
    return redis.error_reply('OUTBOX_ID_COLLISION rejected')
  end
  if input.operation == 'claim_request' and input.expires_ms <= now then return '{"status":"superseded"}' end
  if record.state == 'applied' then
    if input.operation == 'claim_request' then
      if not record.claim_expires_ms then return '{"status":"pending"}' end
      if record.claim_expires_ms <= now then return '{"status":"superseded"}' end
      local kind=redis.call('TYPE',KEYS[9]).ok
      if kind=='none' then return '{"status":"superseded"}' end
      if kind~='hash' then return '{"status":"pending"}' end
      local current=redis.call('HGET',KEYS[9],input.claim_agent)
      if not current then return '{"status":"superseded"}' end
      local valid,claim=pcall(cjson.decode,current)
      local decoded,cached=pcall(cjson.decode,record.response)
      if not valid or not decoded or type(claim)~='table' or type(cached.result)~='table' then return '{"status":"pending"}' end
      if not same(claim,cached.result) then return '{"status":"superseded"}' end
    end
    return record.response
  end
  return '{"status":"pending"}'
end
if input.expires_ms ~= 0 and input.expires_ms <= now then return '{"status":"superseded"}' end
if input.created_ms > now + 300000 then
  return redis.error_reply('OUTBOX_FUTURE_REQUEST rejected')
end

local function require_type(key, expected)
  local actual = redis.call('TYPE', key).ok
  if actual ~= 'none' and actual ~= expected then
    return redis.error_reply('OUTBOX_KEY_TYPE rejected')
  end
  return nil
end
-- Preflight wrong-type errors before reservation or any message side effect.
for index, expected in ipairs(input.key_types) do
  local failure = require_type(KEYS[index], expected)
  if failure then return failure end
end
local retention = input.operation == 'claim_request' and (input.expires_ms - now + 86400000) or nil
local function save(record)
  if retention then redis.call("SET",KEYS[1],cjson.encode(record),"PX",retention)
  else redis.call("SET",KEYS[1],cjson.encode(record)) end
end
local reserved = {state='pending', fingerprint=input.fingerprint}
save(reserved)

local function complete(response)
  save({state='applied',fingerprint=input.fingerprint,response=response})
  return response
end

if input.operation == 'claim_request' then
  -- Existing normal authority code performs the claim once. If it crashes
  -- before saving an exact response, subsequent replay stays PENDING.
  return '{"status":"claim_admission"}'
end

if input.operation == 'presence' then
  local current = redis.call('GET', KEYS[8])
  if current then
    local marker = cjson.decode(current)
    if (marker.origin_id == input.origin_id and marker.sequence >= input.sequence)
      or marker.created_ms > input.created_ms then
      return complete('{"status":"superseded"}')
    end
  end
  -- Ordinary presence writes also guard against resurrecting an older offline
  -- state: their actual server timestamp is compared to original creation.
  local live = redis.call('GET', KEYS[7])
  if live then
    local presence = cjson.decode(live)
    if type(presence.timestamp_utc) == 'string'
      and presence.timestamp_utc > input.created_timestamp then
      return complete('{"status":"superseded"}')
    end
  end
  redis.call('SET', KEYS[7], input.result_json, 'PX', input.expires_ms - now)
  redis.call('SET', KEYS[8], cjson.encode({origin_id=input.origin_id,
    sequence=input.sequence, created_ms=input.created_ms}), 'PX', input.expires_ms - now + 86400000)
  redis.call('PUBLISH', KEYS[3], '{"event":"presence","presence":' .. input.result_json .. '}')
  return complete('{"status":"applied","result":' .. input.result_json .. '}')
end

local stream_id = redis.call('XADD', KEYS[2], 'MAXLEN', '~', input.stream_maxlen,
  '*', unpack(input.message_fields))
-- Preserve Rust's raw JSON arrays/objects. Redis cjson re-encoding empty arrays
-- would change [] into {}, breaking Message.tags and native MCP consumers.
local message_json = string.sub(input.result_json, 1, -2)
  .. ',"stream_id":' .. cjson.encode(stream_id) .. '}'
if input.publish then
  redis.call('PUBLISH', KEYS[3], '{"event":"message","message":' .. message_json .. '}')
end
if input.notification_fields then
  local fields = input.notification_fields
  table.insert(fields, 'message')
  table.insert(fields, message_json)
  redis.call('XADD', KEYS[4], 'MAXLEN', '~', 10000, '*', unpack(fields))
  redis.call('EXPIRE', KEYS[4], 259200)
end
if input.operation == 'ack' then
  redis.call('DEL', KEYS[5], KEYS[6])
elseif input.pending_ack_json then
  redis.call('SET', KEYS[5], input.pending_ack_json, 'EX', 300)
  redis.call('SET', KEYS[6], input.ack_deadline_json, 'EX', input.ack_deadline_seconds)
end
return complete('{"status":"applied","result":' .. message_json .. '}')
