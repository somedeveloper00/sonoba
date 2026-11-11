cbuffer Draw : register(b0) {
    float4 box;
    float2 target_size;
    float2 image_size;
};

float4 main(uint id : SV_VertexID) : SV_POSITION {
    float2 corner = float2(id & 1, id >> 1);
    float2 pixel = box.xy + corner * box.zw;
    return float4(pixel / target_size * float2(2, -2) + float2(-1, 1), 0, 1);
}
