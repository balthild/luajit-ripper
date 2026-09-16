local function bits(a, b)
	local anded = a & b
	local ored = a | b
	local xored = a ~ b
	local notted = ~a
	local shifted_left = a << b
	local shifted_right = a >> b
	local shifted_arith = a ~>> b
	local folded = 0xf0 & 0x0f
	return anded, ored, xored, notted, shifted_left, shifted_right, shifted_arith, folded
end

return bits(0xf0, 3)
