cbuffer Draw : register(b0) {
    float4 box;
    float2 target_size;
    float2 image_size;
};
Texture2D<float4> image : register(t0);
SamplerState nearest : register(s0);
