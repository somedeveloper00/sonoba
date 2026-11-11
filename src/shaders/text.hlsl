float4 main(float4 position : SV_POSITION) : SV_TARGET {
    return image.SampleLevel(nearest, position.xy / image_size, 0);
}
