local small = 1
local negative = -1
local fraction = 1.5
local big = 123456789
local tiny = 1e-10
local huge = 2 ^ 53
local zero = 0
local minustiny = -1e-10

local function arithmetic(a, b)
	return a + b, a - b, a * b, a / b, a % b, a ^ b, -a, #"abc"
end

local function comparisons(a, b)
	if a < b then return 1 end
	if a <= b then return 2 end
	if a > b then return 3 end
	if a >= b then return 4 end
	if a == b then return 5 end
	if a ~= b then return 6 end
	return 0
end

local function constants(index)
	local list = { small, negative, fraction, big, tiny, huge, zero, minustiny }
	return list[index], 100, 3.5, "text", 0.1
end

local function concat(a, b, c)
	return a .. b .. c .. "literal"
end

return {
	arithmetic = arithmetic,
	comparisons = comparisons,
	constants = constants,
	concat = concat,
}
