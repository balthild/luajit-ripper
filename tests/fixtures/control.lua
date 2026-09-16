local function classify(n)
	if n < 0 then
		return "negative"
	elseif n == 0 then
		return "zero"
	elseif n < 10 then
		return "small"
	else
		return "large"
	end
end

local function loops(t, limit)
	local total = 0
	for i = 1, limit do
		if i % 2 == 0 then
			total = total + i
		else
			total = total - 1
		end
	end
	while total > 100 do
		total = total - 1
	end
	repeat
		total = total + 1
	until total > 5
	for _, v in ipairs(t) do
		total = total + v
	end
	for k in pairs(t) do
		if k == "stop" then
			break
		end
		total = total + 1
	end
	while true do
		total = total + 1
		if total > 1000 then
			break
		end
	end
	return total
end

local function guarded(a, b)
	if a and b then
		return 1
	end
	if a or b then
		return 2
	end
	if not (a or b) then
		return 3
	end
	return 4
end

return { classify = classify, loops = loops, guarded = guarded }
