//#region src/zstddec-stream.ts
/**
* Some C structs for reference:
typedef struct ZSTD_inBuffer_s {
const void* src;
size_t size;
size_t pos;
} ZSTD_inBuffer;

typedef struct ZSTD_outBuffer_s {
void*  dst;
size_t size;
size_t pos;
} ZSTD_outBuffer;
*/
let init;
let instance;
let heap;
let heapView;
const IMPORT_OBJECT = { env: { emscripten_notify_memory_growth: (_) => {
	heap = new Uint8Array(instance.exports.memory.buffer);
	heapView = new DataView(heap.buffer);
} } };
/**
* ZSTD (Zstandard) decoder.
*/
var ZSTDDecoder = class {
	init() {
		if (init) return init;
		// gex: the module's wasm a file beside it (zstddec.wasm), not a data URL of
		// it: a page's bundler takes it in as a file.
		const url = new URL("./zstddec.wasm", import.meta.url);
		if (typeof process !== "undefined" && url.protocol === "file:") init = import("node:fs/promises").then((fs) => fs.readFile(url)).then((bytes) => WebAssembly.instantiate(bytes, IMPORT_OBJECT)).then(this._init);
		else init = fetch(url).then((response) => {
			if (!response.ok) throw new Error(`${url}: ${response.status}`);
			return response.arrayBuffer();
		}).then((arrayBuffer) => WebAssembly.instantiate(arrayBuffer, IMPORT_OBJECT)).then(this._init);
		return init;
	}
	/** gex: let the instance and its memory go (a wasm memory never shrinks); the next
	 *  init() makes a new one. Every stream must have ended. */
	dispose() {
		instance = heap = heapView = init = void 0;
	}
	_init(result) {
		instance = result.instance;
		IMPORT_OBJECT.env.emscripten_notify_memory_growth(0);
	}
	decode(array, uncompressedSize = 0) {
		if (!instance) throw new Error("ZSTDDecoder: Await .init() before decoding.");
		const compressedSize = array.byteLength;
		const compressedPtr = instance.exports.malloc(compressedSize);
		heap.set(array, compressedPtr);
		if (uncompressedSize === 0) uncompressedSize = Number(instance.exports.ZSTD_findDecompressedSize(compressedPtr, compressedSize));
		if (uncompressedSize === -1) {
			instance.exports.free(compressedPtr);
			const parts = [];
			for (const out of this.decodeStreaming([array])) parts.push(out);
			if (parts.length === 1) return parts[0];
			const fullByteLength = parts.reduce((acc, arr) => acc + arr.byteLength, 0);
			const result = new Uint8Array(fullByteLength);
			let offset = 0;
			for (const part of parts) {
				result.set(part, offset);
				offset += part.byteLength;
			}
			return result;
		}
		const uncompressedPtr = instance.exports.malloc(uncompressedSize);
		const actualSize = instance.exports.ZSTD_decompress(uncompressedPtr, uncompressedSize, compressedPtr, compressedSize);
		const dec = heap.slice(uncompressedPtr, uncompressedPtr + actualSize);
		instance.exports.free(compressedPtr);
		instance.exports.free(uncompressedPtr);
		return dec;
	}
	/** gex: a push decoder, for decoding while a download arrives. `push(chunk, emit)`
	 *  decodes what `chunk` completes, calling `emit` with each decoded part (a view of the decoder's
	 *  buffer, good until the next call);
	 *  `end()` frees it and throws if the stream is incomplete. */
	stream() {
		if (!instance) throw new Error("ZSTDDecoder: Await .init() before decoding.");
		const buffOutSize = instance.exports.ZSTD_DStreamOutSize();
		const buffOut = instance.exports.malloc(buffOutSize);
		const dctxPtr = instance.exports.ZSTD_createDCtx();
		const inputPtr = instance.exports.malloc(12);
		const outputPtr = instance.exports.malloc(12);
		let lastRet = 0, cap = 0, compressedPtr = 0;
		return {
			push(array, emit) {
				if (array.byteLength > cap) {
					if (compressedPtr) instance.exports.free(compressedPtr);
					cap = array.byteLength;
					compressedPtr = instance.exports.malloc(cap);
				}
				heap.set(array, compressedPtr);
				heapView.setInt32(inputPtr, compressedPtr, true);
				heapView.setInt32(inputPtr + 4, array.byteLength, true);
				heapView.setInt32(inputPtr + 8, 0, true);
				while (heapView.getUint32(inputPtr + 8, true) < heapView.getUint32(inputPtr + 4, true)) {
					heapView.setInt32(outputPtr, buffOut, true);
					heapView.setInt32(outputPtr + 4, buffOutSize, true);
					heapView.setInt32(outputPtr + 8, 0, true);
					lastRet = instance.exports.ZSTD_decompressStream(dctxPtr, outputPtr, inputPtr);
					if (lastRet > 0xffffff00) throw new Error(`ZSTD_decompressStream: error ${lastRet}`);
					const outputPos = heapView.getUint32(outputPtr + 8, true);
					if (outputPos) emit(heap.subarray(buffOut, buffOut + outputPos));
				}
			},
			end() {
				if (compressedPtr) instance.exports.free(compressedPtr);
				instance.exports.ZSTD_freeDCtx(dctxPtr);
				instance.exports.free(buffOut);
				instance.exports.free(inputPtr);
				instance.exports.free(outputPtr);
				if (lastRet !== 0) throw new Error("Incomplete stream, more data expected.");
			}
		};
	}
	*decodeStreaming(arrays) {
		if (!instance) throw new Error("ZSTDDecoder: Await .init() before decoding.");
		const buffInSize = instance.exports.ZSTD_DStreamInSize();
		const buffIn = instance.exports.malloc(buffInSize);
		const buffOutSize = instance.exports.ZSTD_DStreamOutSize();
		const buffOut = instance.exports.malloc(buffOutSize);
		const dctxPtr = instance.exports.ZSTD_createDCtx();
		const sizeOfPointer = 4;
		const sizeOfSizeT = 4;
		const inputPtr = instance.exports.malloc(12);
		const outputPtr = instance.exports.malloc(12);
		let lastRet = 0;
		for (const array of arrays) {
			const compressedPtr = instance.exports.malloc(array.byteLength);
			heap.set(array, compressedPtr);
			heapView.setInt32(inputPtr, compressedPtr, true);
			heapView.setInt32(inputPtr + sizeOfPointer, array.byteLength, true);
			heapView.setInt32(inputPtr + sizeOfPointer + sizeOfSizeT, 0, true);
			while (heapView.getUint32(inputPtr + sizeOfPointer + sizeOfSizeT, true) < heapView.getUint32(inputPtr + sizeOfPointer, true)) {
				heapView.setInt32(outputPtr, buffOut, true);
				heapView.setInt32(outputPtr + sizeOfPointer, buffOutSize, true);
				heapView.setInt32(outputPtr + sizeOfPointer + sizeOfSizeT, 0, true);
				lastRet = instance.exports.ZSTD_decompressStream(dctxPtr, outputPtr, inputPtr);
				const outputPos = heapView.getUint32(outputPtr + sizeOfPointer + sizeOfSizeT, true);
				yield heap.slice(buffOut, buffOut + outputPos);
			}
			instance.exports.free(compressedPtr);
		}
		instance.exports.ZSTD_freeDCtx(dctxPtr);
		instance.exports.free(buffIn);
		instance.exports.free(buffOut);
		instance.exports.free(inputPtr);
		instance.exports.free(outputPtr);
		if (lastRet !== 0) throw new Error("Incomplete stream, more data expected.");
	}
};
/**
* BSD License
*
* For Zstandard software
*
* Copyright (c) 2016-present, Yann Collet, Facebook, Inc. All rights reserved.
*
* Redistribution and use in source and binary forms, with or without modification,
* are permitted provided that the following conditions are met:
*
*  * Redistributions of source code must retain the above copyright notice, this
*    list of conditions and the following disclaimer.
*
*  * Redistributions in binary form must reproduce the above copyright notice,
*    this list of conditions and the following disclaimer in the documentation
*    and/or other materials provided with the distribution.
*
*  * Neither the name Facebook nor the names of its contributors may be used to
*    endorse or promote products derived from this software without specific
*    prior written permission.
*
* THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND
* ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
* WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
* DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR
* ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
* (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
* LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
* ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
* (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
* SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
*/
//#endregion
export { ZSTDDecoder };
