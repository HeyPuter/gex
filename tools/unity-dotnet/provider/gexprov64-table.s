# The module's __table_base (its first slot in gex's function table): a
# provider's own function pointers are [__table_base, + its slot count).
	.globaltype __table_base, i64, immutable
	.globl gexp_table_base
	.type gexp_table_base,@function
gexp_table_base:
	.functype gexp_table_base () -> (i64)
	global.get __table_base
	end_function
